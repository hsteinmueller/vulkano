// Copyright (c) 2022 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use super::{
    AttachmentDescription, AttachmentReference, LoadOp, RenderPass, RenderPassCreateInfo,
    SubpassDependency, SubpassDescription,
};
use crate::{
    device::Device,
    image::{ImageLayout, SampleCount},
    sync::PipelineStages,
    OomError, RequirementNotMet, RequiresOneOf, Version, VulkanError, VulkanObject,
};
use smallvec::SmallVec;
use std::{
    error::Error,
    fmt::{Display, Error as FmtError, Formatter},
    mem::MaybeUninit,
    ptr,
};

impl RenderPass {
    pub(super) fn validate(
        device: &Device,
        create_info: &mut RenderPassCreateInfo,
    ) -> Result<u32, RenderPassCreationError> {
        let properties = device.physical_device().properties();

        let RenderPassCreateInfo {
            attachments,
            subpasses,
            dependencies,
            correlated_view_masks,
            _ne: _,
        } = create_info;

        let mut views_used = 0;

        /*
            Attachments
        */

        let mut attachment_potential_format_features = Vec::with_capacity(attachments.len());

        for (atch_num, attachment) in attachments.iter().enumerate() {
            let &AttachmentDescription {
                format,
                samples,
                load_op,
                store_op,
                stencil_load_op,
                stencil_store_op,
                initial_layout,
                final_layout,
                _ne: _,
            } = attachment;
            let atch_num = atch_num as u32;

            // VUID-VkAttachmentDescription2-finalLayout-03061
            if matches!(
                final_layout,
                ImageLayout::Undefined | ImageLayout::Preinitialized
            ) {
                return Err(RenderPassCreationError::AttachmentLayoutInvalid {
                    attachment: atch_num,
                });
            }

            let format = format.unwrap();
            let aspects = format.aspects();

            // VUID-VkAttachmentDescription2-format-parameter
            format.validate_device(device)?;

            // VUID-VkAttachmentDescription2-samples-parameter
            samples.validate_device(device)?;

            for load_op in [load_op, stencil_load_op] {
                // VUID-VkAttachmentDescription2-loadOp-parameter
                // VUID-VkAttachmentDescription2-stencilLoadOp-parameter
                load_op.validate_device(device)?;
            }

            for store_op in [store_op, stencil_store_op] {
                // VUID-VkAttachmentDescription2-storeOp-parameter
                // VUID-VkAttachmentDescription2-stencilStoreOp-parameter
                store_op.validate_device(device)?;
            }

            for layout in [initial_layout, final_layout] {
                // VUID-VkAttachmentDescription2-initialLayout-parameter
                // VUID-VkAttachmentDescription2-finalLayout-parameter
                layout.validate_device(device)?;

                if aspects.depth || aspects.stencil {
                    // VUID-VkAttachmentDescription2-format-03281
                    // VUID-VkAttachmentDescription2-format-03283
                    if matches!(layout, ImageLayout::ColorAttachmentOptimal) {
                        return Err(RenderPassCreationError::AttachmentLayoutInvalid {
                            attachment: atch_num,
                        });
                    }
                } else {
                    // VUID-VkAttachmentDescription2-format-03280
                    // VUID-VkAttachmentDescription2-format-03282
                    // VUID-VkAttachmentDescription2-format-06487
                    // VUID-VkAttachmentDescription2-format-06488
                    if matches!(
                        layout,
                        ImageLayout::DepthStencilAttachmentOptimal
                            | ImageLayout::DepthStencilReadOnlyOptimal
                            | ImageLayout::DepthAttachmentStencilReadOnlyOptimal
                            | ImageLayout::DepthReadOnlyStencilAttachmentOptimal
                    ) {
                        return Err(RenderPassCreationError::AttachmentLayoutInvalid {
                            attachment: atch_num,
                        });
                    }
                }
            }

            // Use unchecked, because all validation has been done above.
            attachment_potential_format_features.push(unsafe {
                device
                    .physical_device()
                    .format_properties_unchecked(format)
                    .potential_format_features()
            });
        }

        /*
            Subpasses
        */

        // VUID-VkRenderPassCreateInfo2-subpassCount-arraylength
        assert!(!subpasses.is_empty());

        let is_multiview = subpasses[0].view_mask != 0;

        // VUID-VkSubpassDescription2-multiview-06558
        if is_multiview && !device.enabled_features().multiview {
            return Err(RenderPassCreationError::RequirementNotMet {
                required_for: "`create_info.subpasses[0].view_mask` is not `0`",
                requires_one_of: RequiresOneOf {
                    features: &["multiview"],
                    ..Default::default()
                },
            });
        }

        let mut attachment_used = vec![false; attachments.len()];

        for (subpass_num, subpass) in subpasses.iter_mut().enumerate() {
            let &mut SubpassDescription {
                view_mask,
                ref mut input_attachments,
                ref color_attachments,
                ref resolve_attachments,
                ref depth_stencil_attachment,
                ref preserve_attachments,
                _ne: _,
            } = subpass;
            let subpass_num = subpass_num as u32;

            // VUID-VkRenderPassCreateInfo2-viewMask-03058
            if (view_mask != 0) != is_multiview {
                return Err(RenderPassCreationError::SubpassMultiviewMismatch {
                    subpass: subpass_num,
                    multiview: subpass.view_mask != 0,
                    first_subpass_multiview: is_multiview,
                });
            }

            let view_count = u32::BITS - view_mask.leading_zeros();

            // VUID-VkSubpassDescription2-viewMask-06706
            if view_count > properties.max_multiview_view_count.unwrap_or(0) {
                return Err(
                    RenderPassCreationError::SubpassMaxMultiviewViewCountExceeded {
                        subpass: subpass_num,
                        view_count,
                        max: properties.max_multiview_view_count.unwrap_or(0),
                    },
                );
            }

            views_used = views_used.max(view_count);

            // VUID-VkSubpassDescription2-colorAttachmentCount-03063
            if color_attachments.len() as u32 > properties.max_color_attachments {
                return Err(
                    RenderPassCreationError::SubpassMaxColorAttachmentsExceeded {
                        subpass: subpass_num,
                        color_attachment_count: color_attachments.len() as u32,
                        max: properties.max_color_attachments,
                    },
                );
            }

            // Track the layout of each attachment used in this subpass
            let mut layouts = vec![None; attachments.len()];

            // Common checks for all attachment types
            let mut check_attachment = |atch_ref: &AttachmentReference| {
                // VUID-VkAttachmentReference2-layout-parameter
                atch_ref.layout.validate_device(device)?;

                // VUID?
                atch_ref.aspects.validate_device(device)?;

                // VUID-VkRenderPassCreateInfo2-attachment-03051
                let atch = attachments.get(atch_ref.attachment as usize).ok_or(
                    RenderPassCreationError::SubpassAttachmentOutOfRange {
                        subpass: subpass_num,
                        attachment: atch_ref.attachment,
                    },
                )?;

                // VUID-VkSubpassDescription2-layout-02528
                match &mut layouts[atch_ref.attachment as usize] {
                    Some(layout) if *layout == atch_ref.layout => (),
                    Some(_) => {
                        return Err(RenderPassCreationError::SubpassAttachmentLayoutMismatch {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                        })
                    }
                    layout @ None => *layout = Some(atch_ref.layout),
                }

                let first_use =
                    !std::mem::replace(&mut attachment_used[atch_ref.attachment as usize], true);

                if first_use {
                    // VUID-VkRenderPassCreateInfo2-pAttachments-02522
                    if atch.load_op == LoadOp::Clear
                        && matches!(
                            atch_ref.layout,
                            ImageLayout::ShaderReadOnlyOptimal
                                | ImageLayout::DepthStencilReadOnlyOptimal
                                | ImageLayout::DepthReadOnlyStencilAttachmentOptimal
                        )
                    {
                        return Err(RenderPassCreationError::AttachmentFirstUseLoadOpInvalid {
                            attachment: atch_ref.attachment,
                            first_use_subpass: subpass_num,
                        });
                    }

                    // VUID-VkRenderPassCreateInfo2-pAttachments-02523
                    if atch.stencil_load_op == LoadOp::Clear
                        && matches!(
                            atch_ref.layout,
                            ImageLayout::ShaderReadOnlyOptimal
                                | ImageLayout::DepthStencilReadOnlyOptimal
                                | ImageLayout::DepthAttachmentStencilReadOnlyOptimal
                        )
                    {
                        return Err(RenderPassCreationError::AttachmentFirstUseLoadOpInvalid {
                            attachment: atch_ref.attachment,
                            first_use_subpass: subpass_num,
                        });
                    }
                }

                let potential_format_features =
                    &attachment_potential_format_features[atch_ref.attachment as usize];

                Ok((atch, potential_format_features, first_use))
            };

            /*
                Check color attachments
            */

            let mut color_samples = None;

            for atch_ref in color_attachments.iter().flatten() {
                let (atch, features, _first_use) = check_attachment(atch_ref)?;

                // VUID-VkSubpassDescription2-pColorAttachments-02898
                if !features.color_attachment {
                    return Err(
                        RenderPassCreationError::SubpassAttachmentFormatUsageNotSupported {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                            usage: "color",
                        },
                    );
                }

                // VUID-VkAttachmentReference2-layout-03077
                // VUID-VkSubpassDescription2-attachment-06913
                // VUID-VkSubpassDescription2-attachment-06916
                if matches!(
                    atch_ref.layout,
                    ImageLayout::Undefined
                        | ImageLayout::Preinitialized
                        | ImageLayout::PresentSrc
                        | ImageLayout::DepthStencilAttachmentOptimal
                        | ImageLayout::ShaderReadOnlyOptimal
                        | ImageLayout::DepthAttachmentStencilReadOnlyOptimal
                        | ImageLayout::DepthReadOnlyStencilAttachmentOptimal
                ) {
                    return Err(RenderPassCreationError::SubpassAttachmentLayoutInvalid {
                        subpass: subpass_num,
                        attachment: atch_ref.attachment,
                        usage: "color",
                    });
                }

                // Not required by spec, but enforced by Vulkano for sanity.
                if !atch_ref.aspects.is_empty() {
                    return Err(RenderPassCreationError::SubpassAttachmentAspectsNotEmpty {
                        subpass: subpass_num,
                        attachment: atch_ref.attachment,
                    });
                }

                // VUID-VkSubpassDescription2-pColorAttachments-03069
                match &mut color_samples {
                    Some(samples) if *samples == atch.samples => (),
                    Some(samples) => {
                        return Err(
                            RenderPassCreationError::SubpassColorDepthStencilAttachmentSamplesMismatch {
                                subpass: subpass_num,
                                attachment: atch_ref.attachment,
                                samples: atch.samples,
                                first_samples: *samples,
                            },
                        )
                    }
                    samples @ None => *samples = Some(atch.samples),
                }
            }

            /*
                Check depth/stencil attachment
            */

            if let Some(atch_ref) = depth_stencil_attachment.as_ref() {
                let (atch, features, _first_use) = check_attachment(atch_ref)?;

                // VUID-VkSubpassDescription2-pDepthStencilAttachment-02900
                if !features.depth_stencil_attachment {
                    return Err(
                        RenderPassCreationError::SubpassAttachmentFormatUsageNotSupported {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                            usage: "depth/stencil",
                        },
                    );
                }

                // VUID-VkAttachmentReference2-layout-03077
                // VUID-VkSubpassDescription2-attachment-06915
                if matches!(
                    atch_ref.layout,
                    ImageLayout::Undefined
                        | ImageLayout::Preinitialized
                        | ImageLayout::PresentSrc
                        | ImageLayout::ColorAttachmentOptimal
                        | ImageLayout::ShaderReadOnlyOptimal
                ) {
                    return Err(RenderPassCreationError::SubpassAttachmentLayoutInvalid {
                        subpass: subpass_num,
                        attachment: atch_ref.attachment,
                        usage: "depth/stencil",
                    });
                }

                // Not required by spec, but enforced by Vulkano for sanity.
                if !atch_ref.aspects.is_empty() {
                    return Err(RenderPassCreationError::SubpassAttachmentAspectsNotEmpty {
                        subpass: subpass_num,
                        attachment: atch_ref.attachment,
                    });
                }

                // VUID-VkSubpassDescription2-pDepthStencilAttachment-04440
                if color_attachments
                    .iter()
                    .flatten()
                    .any(|color_atch_ref| color_atch_ref.attachment == atch_ref.attachment)
                {
                    return Err(
                        RenderPassCreationError::SubpassAttachmentUsageColorDepthStencil {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                        },
                    );
                }

                // VUID-VkSubpassDescription2-pDepthStencilAttachment-03071
                if let Some(samples) = color_samples.filter(|samples| *samples != atch.samples) {
                    return Err(
                        RenderPassCreationError::SubpassColorDepthStencilAttachmentSamplesMismatch {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                            samples: atch.samples,
                            first_samples: samples,
                        },
                    );
                }
            }

            /*
                Check input attachments
                This must be placed after color and depth/stencil checks so that `first_use`
                will be true for VUID-VkSubpassDescription2-loadOp-03064.
            */

            for atch_ref in input_attachments.iter_mut().flatten() {
                let (atch, features, first_use) = check_attachment(atch_ref)?;

                // VUID-VkSubpassDescription2-pInputAttachments-02897
                if !(features.color_attachment || features.depth_stencil_attachment) {
                    return Err(
                        RenderPassCreationError::SubpassAttachmentFormatUsageNotSupported {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                            usage: "input",
                        },
                    );
                }

                // VUID-VkAttachmentReference2-layout-03077
                // VUID-VkSubpassDescription2-attachment-06912
                if matches!(
                    atch_ref.layout,
                    ImageLayout::Undefined
                        | ImageLayout::Preinitialized
                        | ImageLayout::PresentSrc
                        | ImageLayout::ColorAttachmentOptimal
                        | ImageLayout::DepthStencilAttachmentOptimal
                ) {
                    return Err(RenderPassCreationError::SubpassAttachmentLayoutInvalid {
                        subpass: subpass_num,
                        attachment: atch_ref.attachment,
                        usage: "input",
                    });
                }

                let atch_aspects = atch.format.unwrap().aspects();

                if atch_ref.aspects.is_empty() {
                    // VUID-VkSubpassDescription2-attachment-02800
                    atch_ref.aspects = atch_aspects;
                } else if atch_ref.aspects != atch_aspects {
                    if !(device.api_version() >= Version::V1_1
                        || device.enabled_extensions().khr_create_renderpass2
                        || device.enabled_extensions().khr_maintenance2)
                    {
                        return Err(RenderPassCreationError::RequirementNotMet {
                            required_for: "`create_info.subpasses` has an element, where `input_attachments` has an element that is `Some(atch_ref)`, where `atch_ref.aspects` does not match the aspects of the attachment itself",
                            requires_one_of: RequiresOneOf {
                                api_version: Some(Version::V1_1),
                                device_extensions: &["khr_create_renderpass2", "khr_maintenance2"],
                                ..Default::default()
                            },
                        });
                    }

                    // VUID-VkSubpassDescription2-attachment-02801
                    // VUID-VkSubpassDescription2-attachment-04563
                    // VUID-VkRenderPassCreateInfo2-attachment-02525
                    if !atch_aspects.contains(&atch_ref.aspects) {
                        return Err(
                            RenderPassCreationError::SubpassInputAttachmentAspectsNotCompatible {
                                subpass: subpass_num,
                                attachment: atch_ref.attachment,
                            },
                        );
                    }
                }

                // VUID-VkSubpassDescription2-loadOp-03064
                if first_use && atch.load_op == LoadOp::Clear {
                    return Err(RenderPassCreationError::AttachmentFirstUseLoadOpInvalid {
                        attachment: atch_ref.attachment,
                        first_use_subpass: subpass_num,
                    });
                }
            }

            /*
                Check resolve attachments
            */

            // VUID-VkSubpassDescription2-pResolveAttachments-parameter
            if !(resolve_attachments.is_empty()
                || resolve_attachments.len() == color_attachments.len())
            {
                return Err(
                    RenderPassCreationError::SubpassResolveAttachmentsColorAttachmentsLenMismatch {
                        subpass: subpass_num,
                    },
                );
            }

            for (atch_ref, color_atch_ref) in resolve_attachments
                .iter()
                .zip(subpass.color_attachments.iter())
                .filter_map(|(r, c)| r.as_ref().map(|r| (r, c.as_ref())))
            {
                let (atch, features, _first_use) = check_attachment(atch_ref)?;

                // VUID-VkSubpassDescription2-pResolveAttachments-02899
                if !features.color_attachment {
                    return Err(
                        RenderPassCreationError::SubpassAttachmentFormatUsageNotSupported {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                            usage: "resolve",
                        },
                    );
                }

                // VUID-VkAttachmentReference2-layout-03077
                // VUID-VkSubpassDescription2-attachment-06914
                // VUID-VkSubpassDescription2-attachment-06917
                if matches!(
                    atch_ref.layout,
                    ImageLayout::Undefined
                        | ImageLayout::Preinitialized
                        | ImageLayout::PresentSrc
                        | ImageLayout::DepthStencilAttachmentOptimal
                        | ImageLayout::ShaderReadOnlyOptimal
                        | ImageLayout::DepthAttachmentStencilReadOnlyOptimal
                        | ImageLayout::DepthReadOnlyStencilAttachmentOptimal
                ) {
                    return Err(RenderPassCreationError::SubpassAttachmentLayoutInvalid {
                        subpass: subpass_num,
                        attachment: atch_ref.attachment,
                        usage: "resolve",
                    });
                }

                // Not required by spec, but enforced by Vulkano for sanity.
                if !atch_ref.aspects.is_empty() {
                    return Err(RenderPassCreationError::SubpassAttachmentAspectsNotEmpty {
                        subpass: subpass_num,
                        attachment: atch_ref.attachment,
                    });
                }

                // VUID-VkSubpassDescription2-pResolveAttachments-03065
                let color_atch_ref = color_atch_ref.ok_or({
                    RenderPassCreationError::SubpassResolveAttachmentWithoutColorAttachment {
                        subpass: subpass_num,
                    }
                })?;
                let color_atch = &attachments[color_atch_ref.attachment as usize];

                // VUID-VkSubpassDescription2-pResolveAttachments-03067
                if atch.samples != SampleCount::Sample1 {
                    return Err(
                        RenderPassCreationError::SubpassResolveAttachmentMultisampled {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                        },
                    );
                }

                // VUID-VkSubpassDescription2-pResolveAttachments-03066
                if color_atch.samples == SampleCount::Sample1 {
                    return Err(
                        RenderPassCreationError::SubpassColorAttachmentWithResolveNotMultisampled {
                            subpass: subpass_num,
                            attachment: atch_ref.attachment,
                        },
                    );
                }

                // VUID-VkSubpassDescription2-pResolveAttachments-03068
                if atch.format != color_atch.format {
                    return Err(
                        RenderPassCreationError::SubpassResolveAttachmentFormatMismatch {
                            subpass: subpass_num,
                            resolve_attachment: atch_ref.attachment,
                            color_attachment: color_atch_ref.attachment,
                        },
                    );
                }
            }

            /*
                Check preserve attachments
            */

            for &atch in preserve_attachments {
                // VUID-VkRenderPassCreateInfo2-attachment-03051
                if atch as usize >= attachments.len() {
                    return Err(RenderPassCreationError::SubpassAttachmentOutOfRange {
                        subpass: subpass_num,
                        attachment: atch,
                    });
                }

                // VUID-VkSubpassDescription2-pPreserveAttachments-03074
                if layouts[atch as usize].is_some() {
                    return Err(
                        RenderPassCreationError::SubpassPreserveAttachmentUsedElsewhere {
                            subpass: subpass_num,
                            attachment: atch,
                        },
                    );
                }
            }
        }

        /*
            Dependencies
        */

        for (dependency_num, dependency) in dependencies.iter().enumerate() {
            let &SubpassDependency {
                source_subpass,
                destination_subpass,
                source_stages,
                destination_stages,
                source_access,
                destination_access,
                by_region,
                view_local,
                _ne: _,
            } = dependency;
            let dependency_num = dependency_num as u32;

            for (stages, access) in [
                (source_stages, source_access),
                (destination_stages, destination_access),
            ] {
                // VUID-VkSubpassDependency2-srcStageMask-parameter
                // VUID-VkSubpassDependency2-dstStageMask-parameter
                stages.validate_device(device)?;

                // VUID-VkSubpassDependency2-srcAccessMask-parameter
                // VUID-VkSubpassDependency2-dstAccessMask-parameter
                access.validate_device(device)?;

                // VUID-VkSubpassDependency2-srcStageMask-04090
                // VUID-VkSubpassDependency2-dstStageMask-04090
                if stages.geometry_shader && !device.enabled_features().geometry_shader {
                    return Err(RenderPassCreationError::RequirementNotMet {
                        required_for: "`create_info.dependencies` has an element where `stages.geometry_shader` is set",
                        requires_one_of: RequiresOneOf {
                            features: &["geometry_shader"],
                            ..Default::default()
                        },
                    });
                }

                // VUID-VkSubpassDependency2-srcStageMask-04091
                // VUID-VkSubpassDependency2-dstStageMask-04091
                if (stages.tessellation_control_shader || stages.tessellation_evaluation_shader)
                    && !device.enabled_features().tessellation_shader
                {
                    return Err(RenderPassCreationError::RequirementNotMet {
                        required_for: "`create_info.dependencies` has an element where `stages.tessellation_control_shader` or `stages.tessellation_evaluation_shader` are set",
                        requires_one_of: RequiresOneOf {
                            features: &["tessellation_shader"],
                            ..Default::default()
                        },
                    });
                }

                // VUID-VkSubpassDependency2-srcStageMask-03937
                // VUID-VkSubpassDependency2-dstStageMask-03937
                if stages.is_empty() && !device.enabled_features().synchronization2 {
                    return Err(RenderPassCreationError::RequirementNotMet {
                        required_for:
                            "`create_info.dependencies` has an element where `stages` is empty",
                        requires_one_of: RequiresOneOf {
                            features: &["synchronization2"],
                            ..Default::default()
                        },
                    });
                }

                // VUID-VkSubpassDependency2-srcAccessMask-03088
                // VUID-VkSubpassDependency2-dstAccessMask-03089
                if !stages.supported_access().contains(&access) {
                    return Err(
                        RenderPassCreationError::DependencyAccessNotSupportedByStages {
                            dependency: dependency_num,
                        },
                    );
                }
            }

            // VUID-VkRenderPassCreateInfo2-viewMask-03059
            if view_local.is_some() && !is_multiview {
                return Err(
                    RenderPassCreationError::DependencyViewLocalMultiviewNotEnabled {
                        dependency: dependency_num,
                    },
                );
            }

            // VUID-VkSubpassDependency2-srcSubpass-03085
            if source_subpass.is_none() && destination_subpass.is_none() {
                return Err(RenderPassCreationError::DependencyBothSubpassesExternal {
                    dependency: dependency_num,
                });
            }

            for (subpass, stages) in [
                (source_subpass, source_stages),
                (destination_subpass, destination_stages),
            ] {
                if let Some(subpass) = subpass {
                    // VUID-VkRenderPassCreateInfo2-srcSubpass-02526
                    // VUID-VkRenderPassCreateInfo2-dstSubpass-02527
                    if subpass as usize >= subpasses.len() {
                        return Err(RenderPassCreationError::DependencySubpassOutOfRange {
                            dependency: dependency_num,
                            subpass,
                        });
                    }

                    let remaining_stages = PipelineStages {
                        draw_indirect: false,
                        //index_input: false,
                        //vertex_attribute_input: false,
                        vertex_shader: false,
                        tessellation_control_shader: false,
                        tessellation_evaluation_shader: false,
                        geometry_shader: false,
                        //transform_feedback: false,
                        //fragment_shading_rate_attachment: false,
                        early_fragment_tests: false,
                        fragment_shader: false,
                        late_fragment_tests: false,
                        color_attachment_output: false,
                        all_graphics: false,
                        ..stages
                    };

                    // VUID-VkRenderPassCreateInfo2-pDependencies-03054
                    // VUID-VkRenderPassCreateInfo2-pDependencies-03055
                    if !remaining_stages.is_empty() {
                        return Err(RenderPassCreationError::DependencyStageNotSupported {
                            dependency: dependency_num,
                        });
                    }
                } else {
                    // VUID-VkSubpassDependency2-dependencyFlags-03090
                    // VUID-VkSubpassDependency2-dependencyFlags-03091
                    if view_local.is_some() {
                        return Err(
                            RenderPassCreationError::DependencyViewLocalExternalDependency {
                                dependency: dependency_num,
                            },
                        );
                    }
                }
            }

            if let (Some(source_subpass), Some(destination_subpass)) =
                (source_subpass, destination_subpass)
            {
                // VUID-VkSubpassDependency2-srcSubpass-03084
                if source_subpass > destination_subpass {
                    return Err(
                        RenderPassCreationError::DependencySourceSubpassAfterDestinationSubpass {
                            dependency: dependency_num,
                        },
                    );
                }

                if source_subpass == destination_subpass {
                    let source_stages_non_framebuffer = PipelineStages {
                        early_fragment_tests: false,
                        fragment_shader: false,
                        late_fragment_tests: false,
                        color_attachment_output: false,
                        ..source_stages
                    };
                    let destination_stages_non_framebuffer = PipelineStages {
                        early_fragment_tests: false,
                        fragment_shader: false,
                        late_fragment_tests: false,
                        color_attachment_output: false,
                        ..destination_stages
                    };

                    if !source_stages_non_framebuffer.is_empty()
                        || !destination_stages_non_framebuffer.is_empty()
                    {
                        let source_latest_stage = if source_stages.all_graphics {
                            13
                        } else {
                            let PipelineStages {
                                draw_indirect,
                                //index_input,
                                //vertex_attribute_input,
                                vertex_shader,
                                tessellation_control_shader,
                                tessellation_evaluation_shader,
                                geometry_shader,
                                //transform_feedback,
                                //fragment_shading_rate_attachment,
                                early_fragment_tests,
                                fragment_shader,
                                late_fragment_tests,
                                color_attachment_output,
                                ..
                            } = source_stages;

                            #[allow(clippy::identity_op)]
                            [
                                draw_indirect as u8 * 1,
                                // index_input as u8 * 2,
                                // vertex_attribute_input as u8 * 3,
                                vertex_shader as u8 * 4,
                                tessellation_control_shader as u8 * 5,
                                tessellation_evaluation_shader as u8 * 6,
                                geometry_shader as u8 * 7,
                                // transform_feedback as u8 * 8,
                                // fragment_shading_rate_attachment as u8 * 9,
                                early_fragment_tests as u8 * 10,
                                fragment_shader as u8 * 11,
                                late_fragment_tests as u8 * 12,
                                color_attachment_output as u8 * 13,
                            ]
                            .into_iter()
                            .max()
                            .unwrap()
                        };

                        let destination_earliest_stage = if destination_stages.all_graphics {
                            1
                        } else {
                            let PipelineStages {
                                draw_indirect,
                                //index_input,
                                //vertex_attribute_input,
                                vertex_shader,
                                tessellation_control_shader,
                                tessellation_evaluation_shader,
                                geometry_shader,
                                //transform_feedback,
                                //fragment_shading_rate_attachment,
                                early_fragment_tests,
                                fragment_shader,
                                late_fragment_tests,
                                color_attachment_output,
                                ..
                            } = destination_stages;

                            #[allow(clippy::identity_op)]
                            [
                                draw_indirect as u8 * 1,
                                // index_input as u8 * 2,
                                // vertex_attribute_input as u8 * 3,
                                vertex_shader as u8 * 4,
                                tessellation_control_shader as u8 * 5,
                                tessellation_evaluation_shader as u8 * 6,
                                geometry_shader as u8 * 7,
                                // transform_feedback as u8 * 8,
                                // fragment_shading_rate_attachment as u8 * 9,
                                early_fragment_tests as u8 * 10,
                                fragment_shader as u8 * 11,
                                late_fragment_tests as u8 * 12,
                                color_attachment_output as u8 * 13,
                            ]
                            .into_iter()
                            .min()
                            .unwrap()
                        };

                        // VUID-VkSubpassDependency2-srcSubpass-03087
                        if source_latest_stage > destination_earliest_stage {
                            return Err(
                                RenderPassCreationError::DependencySelfDependencySourceStageAfterDestinationStage {
                                    dependency: dependency_num,
                                },
                            );
                        }
                    }

                    let source_has_framebuffer_stage = source_stages.fragment_shader
                        || source_stages.early_fragment_tests
                        || source_stages.late_fragment_tests
                        || source_stages.color_attachment_output;
                    let destination_has_framebuffer_stage = destination_stages.fragment_shader
                        || destination_stages.early_fragment_tests
                        || destination_stages.late_fragment_tests
                        || destination_stages.color_attachment_output;

                    // VUID-VkSubpassDependency2-srcSubpass-02245
                    if source_has_framebuffer_stage
                        && destination_has_framebuffer_stage
                        && !by_region
                    {
                        return Err(
                            RenderPassCreationError::DependencySelfDependencyFramebufferStagesWithoutByRegion {
                                dependency: dependency_num,
                            },
                        );
                    }

                    if let Some(view_offset) = view_local {
                        // VUID-VkSubpassDependency2-viewOffset-02530
                        if view_offset != 0 {
                            return Err(
                                RenderPassCreationError::DependencySelfDependencyViewLocalNonzeroOffset {
                                    dependency: dependency_num,
                                },
                            );
                        }
                    } else {
                        // VUID-VkRenderPassCreateInfo2-pDependencies-03060
                        if subpasses[source_subpass as usize].view_mask.count_ones() > 1 {
                            return Err(
                                RenderPassCreationError::DependencySelfDependencyViewMaskMultiple {
                                    dependency: dependency_num,
                                    subpass: source_subpass,
                                },
                            );
                        }
                    }
                }
            }
        }

        /*
            Correlated view masks
        */

        // VUID-VkRenderPassCreateInfo2-viewMask-03057
        if !correlated_view_masks.is_empty() {
            if !is_multiview {
                return Err(RenderPassCreationError::CorrelatedViewMasksMultiviewNotEnabled);
            }

            // VUID-VkRenderPassCreateInfo2-pCorrelatedViewMasks-03056
            correlated_view_masks.iter().try_fold(0, |total, &mask| {
                if total & mask != 0 {
                    Err(RenderPassCreationError::CorrelatedViewMasksOverlapping)
                } else {
                    Ok(total | mask)
                }
            })?;
        }

        Ok(views_used)
    }

    pub(super) unsafe fn create_v2(
        device: &Device,
        create_info: &RenderPassCreateInfo,
    ) -> Result<ash::vk::RenderPass, RenderPassCreationError> {
        let RenderPassCreateInfo {
            attachments,
            subpasses,
            dependencies,
            correlated_view_masks,
            _ne: _,
        } = create_info;

        let attachments_vk = attachments
            .iter()
            .map(|attachment| ash::vk::AttachmentDescription2 {
                flags: ash::vk::AttachmentDescriptionFlags::empty(),
                format: attachment
                    .format
                    .map_or(ash::vk::Format::UNDEFINED, |f| f.into()),
                samples: attachment.samples.into(),
                load_op: attachment.load_op.into(),
                store_op: attachment.store_op.into(),
                stencil_load_op: attachment.stencil_load_op.into(),
                stencil_store_op: attachment.stencil_store_op.into(),
                initial_layout: attachment.initial_layout.into(),
                final_layout: attachment.final_layout.into(),
                ..Default::default()
            })
            .collect::<SmallVec<[_; 4]>>();

        let attachment_references_vk = subpasses
            .iter()
            .flat_map(|subpass| {
                (subpass.input_attachments.iter())
                    .chain(subpass.color_attachments.iter())
                    .chain(subpass.resolve_attachments.iter())
                    .map(Option::as_ref)
                    .chain(subpass.depth_stencil_attachment.iter().map(Some))
                    .map(|atch_ref| {
                        if let Some(atch_ref) = atch_ref {
                            ash::vk::AttachmentReference2 {
                                attachment: atch_ref.attachment,
                                layout: atch_ref.layout.into(),
                                aspect_mask: atch_ref.aspects.into(),
                                ..Default::default()
                            }
                        } else {
                            ash::vk::AttachmentReference2 {
                                attachment: ash::vk::ATTACHMENT_UNUSED,
                                ..Default::default()
                            }
                        }
                    })
            })
            .collect::<SmallVec<[_; 8]>>();

        let subpasses_vk = {
            // `ref_index` is increased during the loop and points to the next element to use
            // in `attachment_references_vk`.
            let mut ref_index = 0usize;
            let out: SmallVec<[_; 4]> = subpasses
                .iter()
                .map(|subpass| {
                    let input_attachments = attachment_references_vk.as_ptr().add(ref_index);
                    ref_index += subpass.input_attachments.len();
                    let color_attachments = attachment_references_vk.as_ptr().add(ref_index);
                    ref_index += subpass.color_attachments.len();
                    let resolve_attachments = attachment_references_vk.as_ptr().add(ref_index);
                    ref_index += subpass.resolve_attachments.len();
                    let depth_stencil = if subpass.depth_stencil_attachment.is_some() {
                        let a = attachment_references_vk.as_ptr().add(ref_index);
                        ref_index += 1;
                        a
                    } else {
                        ptr::null()
                    };

                    ash::vk::SubpassDescription2 {
                        flags: ash::vk::SubpassDescriptionFlags::empty(),
                        pipeline_bind_point: ash::vk::PipelineBindPoint::GRAPHICS, // TODO: any need to make this user-specifiable?
                        view_mask: subpass.view_mask,
                        input_attachment_count: subpass.input_attachments.len() as u32,
                        p_input_attachments: if subpass.input_attachments.is_empty() {
                            ptr::null()
                        } else {
                            input_attachments
                        },
                        color_attachment_count: subpass.color_attachments.len() as u32,
                        p_color_attachments: if subpass.color_attachments.is_empty() {
                            ptr::null()
                        } else {
                            color_attachments
                        },
                        p_resolve_attachments: if subpass.resolve_attachments.is_empty() {
                            ptr::null()
                        } else {
                            resolve_attachments
                        },
                        p_depth_stencil_attachment: depth_stencil,
                        preserve_attachment_count: subpass.preserve_attachments.len() as u32,
                        p_preserve_attachments: if subpass.preserve_attachments.is_empty() {
                            ptr::null()
                        } else {
                            subpass.preserve_attachments.as_ptr()
                        },
                        ..Default::default()
                    }
                })
                .collect();

            // If this assertion fails, there's a serious bug in the code above ^.
            debug_assert!(ref_index == attachment_references_vk.len());

            out
        };

        let dependencies_vk = dependencies
            .iter()
            .map(|dependency| {
                let mut dependency_flags = ash::vk::DependencyFlags::empty();

                if dependency.by_region {
                    dependency_flags |= ash::vk::DependencyFlags::BY_REGION;
                }

                if dependency.view_local.is_some() {
                    dependency_flags |= ash::vk::DependencyFlags::VIEW_LOCAL;
                }

                ash::vk::SubpassDependency2 {
                    src_subpass: dependency
                        .source_subpass
                        .unwrap_or(ash::vk::SUBPASS_EXTERNAL),
                    dst_subpass: dependency
                        .destination_subpass
                        .unwrap_or(ash::vk::SUBPASS_EXTERNAL),
                    src_stage_mask: dependency.source_stages.into(),
                    dst_stage_mask: dependency.destination_stages.into(),
                    src_access_mask: dependency.source_access.into(),
                    dst_access_mask: dependency.destination_access.into(),
                    dependency_flags,
                    // VUID-VkSubpassDependency2-dependencyFlags-03092
                    view_offset: dependency.view_local.unwrap_or(0),
                    ..Default::default()
                }
            })
            .collect::<SmallVec<[_; 4]>>();

        let create_info = ash::vk::RenderPassCreateInfo2 {
            flags: ash::vk::RenderPassCreateFlags::empty(),
            attachment_count: attachments_vk.len() as u32,
            p_attachments: if attachments_vk.is_empty() {
                ptr::null()
            } else {
                attachments_vk.as_ptr()
            },
            subpass_count: subpasses_vk.len() as u32,
            p_subpasses: if subpasses_vk.is_empty() {
                ptr::null()
            } else {
                subpasses_vk.as_ptr()
            },
            dependency_count: dependencies_vk.len() as u32,
            p_dependencies: if dependencies_vk.is_empty() {
                ptr::null()
            } else {
                dependencies_vk.as_ptr()
            },
            correlated_view_mask_count: correlated_view_masks.len() as u32,
            p_correlated_view_masks: correlated_view_masks.as_ptr(),
            ..Default::default()
        };

        Ok({
            let fns = device.fns();
            let mut output = MaybeUninit::uninit();

            if device.api_version() >= Version::V1_2 {
                (fns.v1_2.create_render_pass2)(
                    device.internal_object(),
                    &create_info,
                    ptr::null(),
                    output.as_mut_ptr(),
                )
            } else {
                (fns.khr_create_renderpass2.create_render_pass2_khr)(
                    device.internal_object(),
                    &create_info,
                    ptr::null(),
                    output.as_mut_ptr(),
                )
            }
            .result()
            .map_err(VulkanError::from)?;

            output.assume_init()
        })
    }

    pub(super) unsafe fn create_v1(
        device: &Device,
        create_info: &RenderPassCreateInfo,
    ) -> Result<ash::vk::RenderPass, RenderPassCreationError> {
        let RenderPassCreateInfo {
            attachments,
            subpasses,
            dependencies,
            correlated_view_masks,
            _ne: _,
        } = create_info;

        let attachments_vk = attachments
            .iter()
            .map(|attachment| ash::vk::AttachmentDescription {
                flags: ash::vk::AttachmentDescriptionFlags::empty(),
                format: attachment
                    .format
                    .map_or(ash::vk::Format::UNDEFINED, |f| f.into()),
                samples: attachment.samples.into(),
                load_op: attachment.load_op.into(),
                store_op: attachment.store_op.into(),
                stencil_load_op: attachment.stencil_load_op.into(),
                stencil_store_op: attachment.stencil_store_op.into(),
                initial_layout: attachment.initial_layout.into(),
                final_layout: attachment.final_layout.into(),
            })
            .collect::<SmallVec<[_; 4]>>();

        let attachment_references_vk = subpasses
            .iter()
            .flat_map(|subpass| {
                (subpass.input_attachments.iter())
                    .chain(subpass.color_attachments.iter())
                    .chain(subpass.resolve_attachments.iter())
                    .map(Option::as_ref)
                    .chain(subpass.depth_stencil_attachment.iter().map(Some))
                    .map(|atch_ref| {
                        if let Some(atch_ref) = atch_ref {
                            ash::vk::AttachmentReference {
                                attachment: atch_ref.attachment,
                                layout: atch_ref.layout.into(),
                            }
                        } else {
                            ash::vk::AttachmentReference {
                                attachment: ash::vk::ATTACHMENT_UNUSED,
                                layout: Default::default(),
                            }
                        }
                    })
            })
            .collect::<SmallVec<[_; 8]>>();

        let subpasses_vk = {
            // `ref_index` is increased during the loop and points to the next element to use
            // in `attachment_references_vk`.
            let mut ref_index = 0usize;
            let out: SmallVec<[_; 4]> = subpasses
                .iter()
                .map(|subpass| {
                    let input_attachments = attachment_references_vk.as_ptr().add(ref_index);
                    ref_index += subpass.input_attachments.len();
                    let color_attachments = attachment_references_vk.as_ptr().add(ref_index);
                    ref_index += subpass.color_attachments.len();
                    let resolve_attachments = attachment_references_vk.as_ptr().add(ref_index);
                    ref_index += subpass.resolve_attachments.len();
                    let depth_stencil = if subpass.depth_stencil_attachment.is_some() {
                        let a = attachment_references_vk.as_ptr().add(ref_index);
                        ref_index += 1;
                        a
                    } else {
                        ptr::null()
                    };

                    ash::vk::SubpassDescription {
                        flags: ash::vk::SubpassDescriptionFlags::empty(),
                        pipeline_bind_point: ash::vk::PipelineBindPoint::GRAPHICS,
                        input_attachment_count: subpass.input_attachments.len() as u32,
                        p_input_attachments: if subpass.input_attachments.is_empty() {
                            ptr::null()
                        } else {
                            input_attachments
                        },
                        color_attachment_count: subpass.color_attachments.len() as u32,
                        p_color_attachments: if subpass.color_attachments.is_empty() {
                            ptr::null()
                        } else {
                            color_attachments
                        },
                        p_resolve_attachments: if subpass.resolve_attachments.is_empty() {
                            ptr::null()
                        } else {
                            resolve_attachments
                        },
                        p_depth_stencil_attachment: depth_stencil,
                        preserve_attachment_count: subpass.preserve_attachments.len() as u32,
                        p_preserve_attachments: if subpass.preserve_attachments.is_empty() {
                            ptr::null()
                        } else {
                            subpass.preserve_attachments.as_ptr()
                        },
                    }
                })
                .collect();

            // If this assertion fails, there's a serious bug in the code above ^.
            debug_assert!(ref_index == attachment_references_vk.len());

            out
        };

        let dependencies_vk = dependencies
            .iter()
            .map(|dependency| ash::vk::SubpassDependency {
                src_subpass: dependency
                    .source_subpass
                    .unwrap_or(ash::vk::SUBPASS_EXTERNAL),
                dst_subpass: dependency
                    .destination_subpass
                    .unwrap_or(ash::vk::SUBPASS_EXTERNAL),
                src_stage_mask: dependency.source_stages.into(),
                dst_stage_mask: dependency.destination_stages.into(),
                src_access_mask: dependency.source_access.into(),
                dst_access_mask: dependency.destination_access.into(),
                dependency_flags: if dependency.by_region {
                    ash::vk::DependencyFlags::BY_REGION
                } else {
                    ash::vk::DependencyFlags::empty()
                },
            })
            .collect::<SmallVec<[_; 4]>>();

        /* Input attachment aspect */

        let input_attachment_aspect_references: SmallVec<[_; 8]> = if device.api_version()
            >= Version::V1_1
            || device.enabled_extensions().khr_maintenance2
        {
            subpasses
                .iter()
                .enumerate()
                .flat_map(|(subpass_num, subpass)| {
                    subpass.input_attachments.iter().enumerate().flat_map(
                        move |(atch_num, atch_ref)| {
                            atch_ref.as_ref().map(|atch_ref| {
                                ash::vk::InputAttachmentAspectReference {
                                    subpass: subpass_num as u32,
                                    input_attachment_index: atch_num as u32,
                                    aspect_mask: atch_ref.aspects.into(),
                                }
                            })
                        },
                    )
                })
                .collect()
        } else {
            SmallVec::new()
        };

        let mut input_attachment_aspect_create_info =
            if !input_attachment_aspect_references.is_empty() {
                Some(ash::vk::RenderPassInputAttachmentAspectCreateInfo {
                    aspect_reference_count: input_attachment_aspect_references.len() as u32,
                    p_aspect_references: input_attachment_aspect_references.as_ptr(),
                    ..Default::default()
                })
            } else {
                None
            };

        /* Multiview */

        let is_multiview = subpasses[0].view_mask != 0;

        let (multiview_view_masks, multiview_view_offsets): (SmallVec<[_; 4]>, SmallVec<[_; 4]>) =
            if is_multiview {
                (
                    subpasses.iter().map(|subpass| subpass.view_mask).collect(),
                    dependencies
                        .iter()
                        .map(|dependency| dependency.view_local.unwrap_or(0))
                        .collect(),
                )
            } else {
                (SmallVec::new(), SmallVec::new())
            };

        let mut multiview_create_info = if is_multiview {
            debug_assert!(multiview_view_masks.len() == subpasses.len());
            debug_assert!(multiview_view_offsets.len() == dependencies.len());

            Some(ash::vk::RenderPassMultiviewCreateInfo {
                subpass_count: multiview_view_masks.len() as u32,
                p_view_masks: multiview_view_masks.as_ptr(),
                dependency_count: multiview_view_offsets.len() as u32,
                p_view_offsets: multiview_view_offsets.as_ptr(),
                correlation_mask_count: correlated_view_masks.len() as u32,
                p_correlation_masks: correlated_view_masks.as_ptr(),
                ..Default::default()
            })
        } else {
            None
        };

        /* Create */

        let mut create_info = ash::vk::RenderPassCreateInfo {
            flags: ash::vk::RenderPassCreateFlags::empty(),
            attachment_count: attachments_vk.len() as u32,
            p_attachments: if attachments_vk.is_empty() {
                ptr::null()
            } else {
                attachments_vk.as_ptr()
            },
            subpass_count: subpasses_vk.len() as u32,
            p_subpasses: if subpasses_vk.is_empty() {
                ptr::null()
            } else {
                subpasses_vk.as_ptr()
            },
            dependency_count: dependencies_vk.len() as u32,
            p_dependencies: if dependencies_vk.is_empty() {
                ptr::null()
            } else {
                dependencies_vk.as_ptr()
            },
            ..Default::default()
        };

        if let Some(input_attachment_aspect_create_info) =
            input_attachment_aspect_create_info.as_mut()
        {
            input_attachment_aspect_create_info.p_next = create_info.p_next;
            create_info.p_next = input_attachment_aspect_create_info as *const _ as *const _;
        }

        if let Some(multiview_create_info) = multiview_create_info.as_mut() {
            multiview_create_info.p_next = create_info.p_next;
            create_info.p_next = multiview_create_info as *const _ as *const _;
        }

        Ok({
            let fns = device.fns();
            let mut output = MaybeUninit::uninit();
            (fns.v1_0.create_render_pass)(
                device.internal_object(),
                &create_info,
                ptr::null(),
                output.as_mut_ptr(),
            )
            .result()
            .map_err(VulkanError::from)?;
            output.assume_init()
        })
    }
}

/// Error that can happen when creating a `RenderPass`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderPassCreationError {
    /// Not enough memory.
    OomError(OomError),

    RequirementNotMet {
        required_for: &'static str,
        requires_one_of: RequiresOneOf,
    },

    /// An attachment is first used in the render pass with a read-only layout or as an input
    /// attachment, but its `load_op` or `stencil_load_op` is [`LoadOp::Clear`].
    AttachmentFirstUseLoadOpInvalid {
        attachment: u32,
        first_use_subpass: u32,
    },

    /// An attachment has an `initial_layout` or `final_layout` value that is invalid for the
    /// provided `format`.
    AttachmentLayoutInvalid { attachment: u32 },

    /// Correlated view masks were included, but multiview is not enabled on the render pass.
    CorrelatedViewMasksMultiviewNotEnabled,

    /// The provided correlated view masks contain a bit that is set in more than one element.
    CorrelatedViewMasksOverlapping,

    /// A subpass dependency specified an access type that was not supported by the given stages.
    DependencyAccessNotSupportedByStages { dependency: u32 },

    /// A subpass dependency has both `source_subpass` and `destination_subpass` set to `None`.
    DependencyBothSubpassesExternal { dependency: u32 },

    /// A subpass dependency specifies a subpass self-dependency and includes framebuffer stages in
    /// both `source_stages` and `destination_stages`, but the `by_region` dependency was not
    /// enabled.
    DependencySelfDependencyFramebufferStagesWithoutByRegion { dependency: u32 },

    /// A subpass dependency specifies a subpass self-dependency and includes
    /// non-framebuffer stages, but the latest stage in `source_stages` is after the earliest stage
    /// in `destination_stages`.
    DependencySelfDependencySourceStageAfterDestinationStage { dependency: u32 },

    /// A subpass dependency specifies a subpass self-dependency and has the `view_local` dependency
    /// enabled, but the inner offset value was not 0.
    DependencySelfDependencyViewLocalNonzeroOffset { dependency: u32 },

    /// A subpass dependency specifies a subpass self-dependency without the `view_local`
    /// dependency, but the referenced subpass has more than one bit set in its `view_mask`.
    DependencySelfDependencyViewMaskMultiple { dependency: u32, subpass: u32 },

    /// A subpass dependency has a `source_subpass` that is later than the `destination_subpass`.
    DependencySourceSubpassAfterDestinationSubpass { dependency: u32 },

    /// A subpass dependency has a bit set in the `source_stages` or `destination_stages` that is
    /// not supported for graphics pipelines.
    DependencyStageNotSupported { dependency: u32 },

    /// A subpass index in a subpass dependency is not less than the number of subpasses in the
    /// render pass.
    DependencySubpassOutOfRange { dependency: u32, subpass: u32 },

    /// A subpass dependency has the `view_local` dependency enabled, but `source_subpass` or
    /// `destination_subpass` were set to `None`.
    DependencyViewLocalExternalDependency { dependency: u32 },

    /// A subpass dependency has the `view_local` dependency enabled, but multiview is not enabled
    /// on the render pass.
    DependencyViewLocalMultiviewNotEnabled { dependency: u32 },

    /// A reference to an attachment used other than as an input attachment in a subpass has
    /// one or more aspects selected.
    SubpassAttachmentAspectsNotEmpty { subpass: u32, attachment: u32 },

    /// An attachment used as an attachment in a subpass has a layout that is not supported for
    /// that usage.
    SubpassAttachmentLayoutInvalid {
        subpass: u32,
        attachment: u32,
        usage: &'static str,
    },

    /// The layouts of all uses of an attachment in a subpass do not match.
    SubpassAttachmentLayoutMismatch { subpass: u32, attachment: u32 },

    /// An attachment index in a subpass is not less than the number of attachments in the render
    /// pass.
    SubpassAttachmentOutOfRange { subpass: u32, attachment: u32 },

    /// An attachment is used as both a color attachment and a depth/stencil attachment in a
    /// subpass.
    SubpassAttachmentUsageColorDepthStencil { subpass: u32, attachment: u32 },

    /// An attachment used as an attachment in a subpass has a format that does not support that
    /// usage.
    SubpassAttachmentFormatUsageNotSupported {
        subpass: u32,
        attachment: u32,
        usage: &'static str,
    },

    /// An attachment used as a color attachment in a subpass with resolve attachments has a
    /// `samples` value of [`SampleCount::Sample1`].
    SubpassColorAttachmentWithResolveNotMultisampled { subpass: u32, attachment: u32 },

    /// An attachment used as a color or depth/stencil attachment in a subpass has a `samples` value
    /// that is different from the first color attachment.
    SubpassColorDepthStencilAttachmentSamplesMismatch {
        subpass: u32,
        attachment: u32,
        samples: SampleCount,
        first_samples: SampleCount,
    },

    /// A reference to an attachment used as an input attachment in a subpass selects aspects that
    /// are not present in the format of the attachment.
    SubpassInputAttachmentAspectsNotCompatible { subpass: u32, attachment: u32 },

    /// The `max_color_attachments` limit has been exceeded for a subpass.
    SubpassMaxColorAttachmentsExceeded {
        subpass: u32,
        color_attachment_count: u32,
        max: u32,
    },

    /// The `max_multiview_view_count` limit has been exceeded for a subpass.
    SubpassMaxMultiviewViewCountExceeded {
        subpass: u32,
        view_count: u32,
        max: u32,
    },

    /// The multiview state (whether `view_mask` is nonzero) of a subpass is different from the
    /// first subpass.
    SubpassMultiviewMismatch {
        subpass: u32,
        multiview: bool,
        first_subpass_multiview: bool,
    },

    /// An attachment marked as a preserve attachment in a subpass is also used as an attachment
    /// in that subpass.
    SubpassPreserveAttachmentUsedElsewhere { subpass: u32, attachment: u32 },

    /// The `resolve_attachments` field of a subpass was not empty, but its length did not match
    /// the length of `color_attachments`.
    SubpassResolveAttachmentsColorAttachmentsLenMismatch { subpass: u32 },

    /// An attachment used as a resolve attachment in a subpass has a `format` value different from
    /// the corresponding color attachment.
    SubpassResolveAttachmentFormatMismatch {
        subpass: u32,
        resolve_attachment: u32,
        color_attachment: u32,
    },

    /// An attachment used as a resolve attachment in a subpass has a `samples` value other than
    /// [`SampleCount::Sample1`].
    SubpassResolveAttachmentMultisampled { subpass: u32, attachment: u32 },

    /// A resolve attachment in a subpass is `Some`, but the corresponding color attachment is
    /// `None`.
    SubpassResolveAttachmentWithoutColorAttachment { subpass: u32 },
}

impl Error for RenderPassCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            RenderPassCreationError::OomError(err) => Some(err),
            _ => None,
        }
    }
}

impl Display for RenderPassCreationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self {
            Self::OomError(_) => write!(f, "not enough memory available"),
            Self::RequirementNotMet {
                required_for,
                requires_one_of,
            } => write!(
                f,
                "a requirement was not met for: {}; requires one of: {}",
                required_for, requires_one_of,
            ),
            Self::AttachmentFirstUseLoadOpInvalid {
                attachment,
                first_use_subpass,
            } => write!(
                f,
                "attachment {} is first used in the render pass in subpass {} with a read-only \
                layout or as an input attachment, but its `load_op` or `stencil_load_op` is \
                `LoadOp::Clear`",
                attachment, first_use_subpass,
            ),
            Self::AttachmentLayoutInvalid { attachment } => write!(
                f,
                "attachment {} has an `initial_layout` or `final_layout` value that is invalid for \
                the provided `format`",
                attachment,
            ),
            Self::CorrelatedViewMasksMultiviewNotEnabled => write!(
                f,
                "correlated view masks were included, but multiview is not enabled on the render \
                pass",
            ),
            Self::CorrelatedViewMasksOverlapping => write!(
                f,
                "the provided correlated view masks contain a bit that is set in more than one \
                element",
            ),
            Self::DependencyAccessNotSupportedByStages { dependency } => write!(
                f,
                "subpass dependency {} specified an access type that was not supported by the \
                given stages",
                dependency,
            ),
            Self::DependencySelfDependencyFramebufferStagesWithoutByRegion { dependency } => {
                write!(
                    f,
                    "subpass dependency {} specifies a subpass self-dependency and includes \
                    framebuffer stages in both `source_stages` and `destination_stages`, but the \
                    `by_region` dependency was not enabled",
                    dependency,
                )
            }
            Self::DependencySelfDependencySourceStageAfterDestinationStage { dependency } => {
                write!(
                    f,
                    "subpass dependency {} specifies a subpass self-dependency and includes \
                    non-framebuffer stages, but the latest stage in `source_stages` is after the \
                    earliest stage in `destination_stages`",
                    dependency,
                )
            }
            Self::DependencySelfDependencyViewLocalNonzeroOffset { dependency } => write!(
                f,
                "subpass dependency {} specifies a subpass self-dependency and has the \
                `view_local` dependency enabled, but the inner offset value was not 0",
                dependency,
            ),
            Self::DependencySelfDependencyViewMaskMultiple {
                dependency,
                subpass,
            } => write!(
                f,
                "subpass dependency {} specifies a subpass self-dependency without the \
                `view_local` dependency, but the referenced subpass {} has more than one bit set \
                in its `view_mask`",
                dependency, subpass,
            ),
            Self::DependencySourceSubpassAfterDestinationSubpass { dependency } => write!(
                f,
                "subpass dependency {} has a `source_subpass` that is later than the \
                `destination_subpass`",
                dependency,
            ),
            Self::DependencyStageNotSupported { dependency } => write!(
                f,
                "subpass dependency {} has a bit set in the `source_stages` or \
                `destination_stages` that is not supported for graphics pipelines",
                dependency,
            ),
            Self::DependencyBothSubpassesExternal { dependency } => write!(
                f,
                "subpass dependency {} has both `source_subpass` and `destination_subpass` set to \
                `None`",
                dependency,
            ),
            Self::DependencySubpassOutOfRange {
                dependency,
                subpass,
            } => write!(
                f,
                "the subpass index {} in subpass dependency {} is not less than the number of \
                subpasses in the render pass",
                subpass, dependency,
            ),
            Self::DependencyViewLocalExternalDependency { dependency } => write!(
                f,
                "subpass dependency {} has the `view_local` dependency enabled, but \
                `source_subpass` or `destination_subpass` were set to `None`",
                dependency,
            ),
            Self::DependencyViewLocalMultiviewNotEnabled { dependency } => write!(
                f,
                "subpass dependency {} has the `view_local` dependency enabled, but multiview is \
                not enabled on the render pass",
                dependency,
            ),
            Self::SubpassAttachmentAspectsNotEmpty {
                subpass,
                attachment,
            } => write!(
                f,
                "a reference to attachment {} used other than as an input attachment in subpass {} \
                has one or more aspects selected",
                attachment, subpass,
            ),
            Self::SubpassAttachmentLayoutMismatch {
                subpass,
                attachment,
            } => write!(
                f,
                "the layouts of all uses of attachment {} in subpass {} do not match.",
                attachment, subpass,
            ),
            Self::SubpassAttachmentLayoutInvalid {
                subpass,
                attachment,
                usage,
            } => write!(
                f,
                "attachment {} used as {} attachment in subpass {} has a layout that is not \
                supported for that usage",
                attachment, usage, subpass,
            ),
            Self::SubpassAttachmentOutOfRange {
                subpass,
                attachment,
            } => write!(
                f,
                "the attachment index {} in subpass {} is not less than the number of attachments \
                in the render pass",
                attachment, subpass,
            ),
            Self::SubpassAttachmentUsageColorDepthStencil {
                subpass,
                attachment,
            } => write!(
                f,
                "attachment {} is used as both a color attachment and a depth/stencil attachment \
                in subpass {}",
                attachment, subpass,
            ),
            Self::SubpassAttachmentFormatUsageNotSupported {
                subpass,
                attachment,
                usage,
            } => write!(
                f,
                "attachment {} used as {} attachment in subpass {} has a format that does not \
                support that usage",
                attachment, usage, subpass,
            ),
            Self::SubpassColorAttachmentWithResolveNotMultisampled {
                subpass,
                attachment,
            } => write!(
                f,
                "attachment {} used as a color attachment in subpass {} with resolve attachments \
                has a `samples` value of `SampleCount::Sample1`",
                attachment, subpass,
            ),
            Self::SubpassColorDepthStencilAttachmentSamplesMismatch {
                subpass,
                attachment,
                samples,
                first_samples,
            } => write!(
                f,
                "attachment {} used as a color or depth/stencil attachment in subpass {} has a \
                `samples` value {:?} that is different from the first color attachment ({:?})",
                attachment, subpass, samples, first_samples,
            ),
            Self::SubpassInputAttachmentAspectsNotCompatible {
                subpass,
                attachment,
            } => write!(
                f,
                "a reference to attachment {} used as an input attachment in subpass {} selects \
                aspects that are not present in the format of the attachment",
                attachment, subpass,
            ),
            Self::SubpassMaxColorAttachmentsExceeded { .. } => {
                write!(f, "the `max_color_attachments` limit has been exceeded")
            }
            Self::SubpassMaxMultiviewViewCountExceeded { .. } => write!(
                f,
                "the `max_multiview_view_count` limit has been exceeded for a subpass",
            ),
            Self::SubpassMultiviewMismatch {
                subpass,
                multiview,
                first_subpass_multiview,
            } => write!(
                f,
                "the multiview state (whether `view_mask` is nonzero) of subpass {} is {}, which \
                is different from the first subpass ({})",
                subpass, multiview, first_subpass_multiview,
            ),
            Self::SubpassPreserveAttachmentUsedElsewhere {
                subpass,
                attachment,
            } => write!(
                f,
                "attachment {} marked as a preserve attachment in subpass {} is also used as an \
                attachment in that subpass",
                attachment, subpass,
            ),
            Self::SubpassResolveAttachmentsColorAttachmentsLenMismatch { subpass } => write!(
                f,
                "the `resolve_attachments` field of subpass {} was not empty, but its length did \
                not match the length of `color_attachments`",
                subpass,
            ),
            Self::SubpassResolveAttachmentFormatMismatch {
                subpass,
                resolve_attachment,
                color_attachment,
            } => write!(
                f,
                "attachment {} used as a resolve attachment in subpass {} has a `format` value \
                different from the corresponding color attachment {}",
                subpass, resolve_attachment, color_attachment,
            ),
            Self::SubpassResolveAttachmentMultisampled {
                subpass,
                attachment,
            } => write!(
                f,
                "attachment {} used as a resolve attachment in subpass {} has a `samples` value \
                other than `SampleCount::Sample1`",
                attachment, subpass,
            ),
            Self::SubpassResolveAttachmentWithoutColorAttachment { subpass } => write!(
                f,
                "a resolve attachment in subpass {} is `Some`, but the corresponding color \
                attachment is `None`",
                subpass,
            ),
        }
    }
}

impl From<OomError> for RenderPassCreationError {
    fn from(err: OomError) -> RenderPassCreationError {
        RenderPassCreationError::OomError(err)
    }
}

impl From<VulkanError> for RenderPassCreationError {
    fn from(err: VulkanError) -> RenderPassCreationError {
        match err {
            err @ VulkanError::OutOfHostMemory => {
                RenderPassCreationError::OomError(OomError::from(err))
            }
            err @ VulkanError::OutOfDeviceMemory => {
                RenderPassCreationError::OomError(OomError::from(err))
            }
            _ => panic!("unexpected error: {:?}", err),
        }
    }
}

impl From<RequirementNotMet> for RenderPassCreationError {
    fn from(err: RequirementNotMet) -> Self {
        Self::RequirementNotMet {
            required_for: err.required_for,
            requires_one_of: err.requires_one_of,
        }
    }
}
