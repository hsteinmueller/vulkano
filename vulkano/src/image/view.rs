// Copyright (c) 2021 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! Image views.
//!
//! This module contains types related to image views. An image view wraps around
//! an image and describes how the GPU should interpret the data. It is needed when an image is
//! to be used in a shader descriptor or as a framebuffer attachment.

use super::{
    sys::UnsafeImage, ImageAccess, ImageDimensions, ImageFormatInfo, ImageSubresourceRange,
    ImageUsage,
};
use crate::{
    device::{Device, DeviceOwned},
    format::{ChromaSampling, Format, FormatFeatures},
    image::{ImageAspects, ImageTiling, ImageType, SampleCount},
    macros::vulkan_enum,
    sampler::{ycbcr::SamplerYcbcrConversion, ComponentMapping},
    OomError, RequirementNotMet, RequiresOneOf, Version, VulkanError, VulkanObject,
};
use std::{
    error::Error,
    fmt::{Debug, Display, Error as FmtError, Formatter},
    hash::{Hash, Hasher},
    mem::MaybeUninit,
    ptr,
    sync::Arc,
};

/// A wrapper around an image that makes it available to shaders or framebuffers.
#[derive(Debug)]
pub struct ImageView<I>
where
    I: ImageAccess + ?Sized,
{
    handle: ash::vk::ImageView,
    image: Arc<I>,

    component_mapping: ComponentMapping,
    format: Option<Format>,
    format_features: FormatFeatures,
    sampler_ycbcr_conversion: Option<Arc<SamplerYcbcrConversion>>,
    subresource_range: ImageSubresourceRange,
    usage: ImageUsage,
    view_type: ImageViewType,

    filter_cubic: bool,
    filter_cubic_minmax: bool,
}

impl<I> ImageView<I>
where
    I: ImageAccess + ?Sized,
{
    /// Creates a new `ImageView`.
    ///
    /// # Panics
    ///
    /// - Panics if `create_info.array_layers` is empty.
    /// - Panics if `create_info.mip_levels` is empty.
    /// - Panics if `create_info.aspects` contains any aspects other than `color`, `depth`,
    ///   `stencil`, `plane0`, `plane1` or `plane2`.
    /// - Panics if `create_info.aspects` contains more more than one aspect, unless `depth` and
    ///   `stencil` are the only aspects selected.
    pub fn new(
        image: Arc<I>,
        create_info: ImageViewCreateInfo,
    ) -> Result<Arc<ImageView<I>>, ImageViewCreationError> {
        let format_features = Self::validate_new(&image, &create_info)?;

        unsafe {
            Ok(Self::new_unchecked_with_format_features(
                image,
                create_info,
                format_features,
            )?)
        }
    }

    fn validate_new(
        image: &I,
        create_info: &ImageViewCreateInfo,
    ) -> Result<FormatFeatures, ImageViewCreationError> {
        let &ImageViewCreateInfo {
            view_type,
            format,
            component_mapping,
            ref subresource_range,
            mut usage,
            ref sampler_ycbcr_conversion,
            _ne: _,
        } = create_info;

        let image_inner = image.inner().image;
        let device = image_inner.device();
        let format = format.unwrap();

        let level_count = subresource_range.mip_levels.end - subresource_range.mip_levels.start;
        let layer_count = subresource_range.array_layers.end - subresource_range.array_layers.start;

        // VUID-VkImageSubresourceRange-aspectMask-requiredbitmask
        assert!(!subresource_range.aspects.is_empty());

        // VUID-VkImageSubresourceRange-levelCount-01720
        assert!(level_count != 0);

        // VUID-VkImageSubresourceRange-layerCount-01721
        assert!(layer_count != 0);

        let default_usage = Self::get_default_usage(subresource_range.aspects, image_inner);

        let has_non_default_usage = if usage.is_empty() {
            usage = default_usage;
            false
        } else {
            usage == default_usage
        };

        // VUID-VkImageViewCreateInfo-viewType-parameter
        view_type.validate_device(device)?;

        // VUID-VkImageViewCreateInfo-format-parameter
        format.validate_device(device)?;

        // VUID-VkComponentMapping-r-parameter
        component_mapping.r.validate_device(device)?;

        // VUID-VkComponentMapping-g-parameter
        component_mapping.g.validate_device(device)?;

        // VUID-VkComponentMapping-b-parameter
        component_mapping.b.validate_device(device)?;

        // VUID-VkComponentMapping-a-parameter
        component_mapping.a.validate_device(device)?;

        // VUID-VkImageSubresourceRange-aspectMask-parameter
        subresource_range.aspects.validate_device(device)?;

        {
            let ImageAspects {
                color,
                depth,
                stencil,
                metadata,
                plane0,
                plane1,
                plane2,
                memory_plane0,
                memory_plane1,
                memory_plane2,
                _ne: _,
            } = subresource_range.aspects;

            assert!(!(metadata || memory_plane0 || memory_plane1 || memory_plane2));
            assert!({
                let num_bits = color as u8
                    + depth as u8
                    + stencil as u8
                    + plane0 as u8
                    + plane1 as u8
                    + plane2 as u8;
                num_bits == 1 || depth && stencil && !(color || plane0 || plane1 || plane2)
            });
        }

        // Get format features
        let format_features = unsafe { Self::get_format_features(format, image_inner) };

        // No VUID apparently, but this seems like something we want to check?
        if !image_inner
            .format()
            .unwrap()
            .aspects()
            .contains(&subresource_range.aspects)
        {
            return Err(ImageViewCreationError::ImageAspectsNotCompatible {
                aspects: subresource_range.aspects,
                image_aspects: image_inner.format().unwrap().aspects(),
            });
        }

        // VUID-VkImageViewCreateInfo-None-02273
        if format_features == FormatFeatures::default() {
            return Err(ImageViewCreationError::FormatNotSupported);
        }

        // Check for compatibility with the image
        let image_type = image.dimensions().image_type();

        // VUID-VkImageViewCreateInfo-subResourceRange-01021
        if !view_type.is_compatible_with(image_type) {
            return Err(ImageViewCreationError::ImageTypeNotCompatible);
        }

        // VUID-VkImageViewCreateInfo-image-01003
        if (view_type == ImageViewType::Cube || view_type == ImageViewType::CubeArray)
            && !image_inner.cube_compatible()
        {
            return Err(ImageViewCreationError::ImageNotCubeCompatible);
        }

        // VUID-VkImageViewCreateInfo-viewType-01004
        if view_type == ImageViewType::CubeArray && !device.enabled_features().image_cube_array {
            return Err(ImageViewCreationError::RequirementNotMet {
                required_for: "`create_info.viewtype` is `ImageViewType::CubeArray`",
                requires_one_of: RequiresOneOf {
                    features: &["image_cube_array"],
                    ..Default::default()
                },
            });
        }

        // VUID-VkImageViewCreateInfo-subresourceRange-01718
        if subresource_range.mip_levels.end > image_inner.mip_levels() {
            return Err(ImageViewCreationError::MipLevelsOutOfRange {
                range_end: subresource_range.mip_levels.end,
                max: image_inner.mip_levels(),
            });
        }

        if image_type == ImageType::Dim3d
            && (view_type == ImageViewType::Dim2d || view_type == ImageViewType::Dim2dArray)
        {
            // VUID-VkImageViewCreateInfo-image-01005
            if !image_inner.array_2d_compatible() {
                return Err(ImageViewCreationError::ImageNotArray2dCompatible);
            }

            // VUID-VkImageViewCreateInfo-image-04970
            if level_count != 1 {
                return Err(ImageViewCreationError::Array2dCompatibleMultipleMipLevels);
            }

            // VUID-VkImageViewCreateInfo-image-02724
            // VUID-VkImageViewCreateInfo-subresourceRange-02725
            // We're using the depth dimension as array layers, but because of mip scaling, the
            // depth, and therefore number of layers available, shrinks as the mip level gets
            // higher.
            let max = image_inner
                .dimensions()
                .mip_level_dimensions(subresource_range.mip_levels.start)
                .unwrap()
                .depth();
            if subresource_range.array_layers.end > max {
                return Err(ImageViewCreationError::ArrayLayersOutOfRange {
                    range_end: subresource_range.array_layers.end,
                    max,
                });
            }
        } else {
            // VUID-VkImageViewCreateInfo-image-01482
            // VUID-VkImageViewCreateInfo-subresourceRange-01483
            if subresource_range.array_layers.end > image_inner.dimensions().array_layers() {
                return Err(ImageViewCreationError::ArrayLayersOutOfRange {
                    range_end: subresource_range.array_layers.end,
                    max: image_inner.dimensions().array_layers(),
                });
            }
        }

        // VUID-VkImageViewCreateInfo-image-04972
        if image_inner.samples() != SampleCount::Sample1
            && !(view_type == ImageViewType::Dim2d || view_type == ImageViewType::Dim2dArray)
        {
            return Err(ImageViewCreationError::MultisamplingNot2d);
        }

        /* Check usage requirements */

        if has_non_default_usage {
            if !(device.api_version() >= Version::V1_1
                || device.enabled_extensions().khr_maintenance2)
            {
                return Err(ImageViewCreationError::RequirementNotMet {
                    required_for: "`create_info.usage` is not the default value",
                    requires_one_of: RequiresOneOf {
                        api_version: Some(Version::V1_1),
                        device_extensions: &["khr_maintenance2"],
                        ..Default::default()
                    },
                });
            }

            // VUID-VkImageViewUsageCreateInfo-usage-parameter
            usage.validate_device(device)?;

            // VUID-VkImageViewUsageCreateInfo-usage-requiredbitmask
            assert!(!usage.is_empty());

            // VUID-VkImageViewCreateInfo-pNext-02662
            // VUID-VkImageViewCreateInfo-pNext-02663
            // VUID-VkImageViewCreateInfo-pNext-02664
            if !default_usage.contains(&usage) {
                return Err(ImageViewCreationError::UsageNotSupportedByImage {
                    usage,
                    supported_usage: default_usage,
                });
            }
        }

        // VUID-VkImageViewCreateInfo-image-04441
        if !(image_inner.usage().sampled
            || image_inner.usage().storage
            || image_inner.usage().color_attachment
            || image_inner.usage().depth_stencil_attachment
            || image_inner.usage().input_attachment
            || image_inner.usage().transient_attachment)
        {
            return Err(ImageViewCreationError::ImageMissingUsage);
        }

        // VUID-VkImageViewCreateInfo-usage-02274
        if usage.sampled && !format_features.sampled_image {
            return Err(ImageViewCreationError::FormatUsageNotSupported { usage: "sampled" });
        }

        // VUID-VkImageViewCreateInfo-usage-02275
        if usage.storage && !format_features.storage_image {
            return Err(ImageViewCreationError::FormatUsageNotSupported { usage: "storage" });
        }

        // VUID-VkImageViewCreateInfo-usage-02276
        if usage.color_attachment && !format_features.color_attachment {
            return Err(ImageViewCreationError::FormatUsageNotSupported {
                usage: "color_attachment",
            });
        }

        // VUID-VkImageViewCreateInfo-usage-02277
        if usage.depth_stencil_attachment && !format_features.depth_stencil_attachment {
            return Err(ImageViewCreationError::FormatUsageNotSupported {
                usage: "depth_stencil_attachment",
            });
        }

        // VUID-VkImageViewCreateInfo-usage-02652
        if usage.input_attachment
            && !(format_features.color_attachment || format_features.depth_stencil_attachment)
        {
            return Err(ImageViewCreationError::FormatUsageNotSupported {
                usage: "input_attachment",
            });
        }

        /* Check flags requirements */

        if image_inner.block_texel_view_compatible() {
            // VUID-VkImageViewCreateInfo-image-01583
            if !(format.compatibility() == image_inner.format().unwrap().compatibility()
                || format.block_size() == image_inner.format().unwrap().block_size())
            {
                return Err(ImageViewCreationError::FormatNotCompatible);
            }

            // VUID-VkImageViewCreateInfo-image-01584
            if layer_count != 1 {
                return Err(ImageViewCreationError::BlockTexelViewCompatibleMultipleArrayLayers);
            }

            // VUID-VkImageViewCreateInfo-image-01584
            if level_count != 1 {
                return Err(ImageViewCreationError::BlockTexelViewCompatibleMultipleMipLevels);
            }

            // VUID-VkImageViewCreateInfo-image-04739
            if format.compression().is_none() && view_type == ImageViewType::Dim3d {
                return Err(ImageViewCreationError::BlockTexelViewCompatibleUncompressedIs3d);
            }
        }
        // VUID-VkImageViewCreateInfo-image-01761
        else if image_inner.mutable_format()
            && image_inner.format().unwrap().planes().is_empty()
            && format.compatibility() != image_inner.format().unwrap().compatibility()
        {
            return Err(ImageViewCreationError::FormatNotCompatible);
        }

        if image_inner.mutable_format()
            && !image_inner.format().unwrap().planes().is_empty()
            && !subresource_range.aspects.color
        {
            let plane = if subresource_range.aspects.plane0 {
                0
            } else if subresource_range.aspects.plane1 {
                1
            } else if subresource_range.aspects.plane2 {
                2
            } else {
                unreachable!()
            };
            let plane_format = image_inner.format().unwrap().planes()[plane];

            // VUID-VkImageViewCreateInfo-image-01586
            if format.compatibility() != plane_format.compatibility() {
                return Err(ImageViewCreationError::FormatNotCompatible);
            }
        }
        // VUID-VkImageViewCreateInfo-image-01762
        else if Some(format) != image_inner.format() {
            return Err(ImageViewCreationError::FormatNotCompatible);
        }

        // VUID-VkImageViewCreateInfo-imageViewType-04973
        if (view_type == ImageViewType::Dim1d
            || view_type == ImageViewType::Dim2d
            || view_type == ImageViewType::Dim3d)
            && layer_count != 1
        {
            return Err(ImageViewCreationError::TypeNonArrayedMultipleArrayLayers);
        }
        // VUID-VkImageViewCreateInfo-viewType-02960
        else if view_type == ImageViewType::Cube && layer_count != 6 {
            return Err(ImageViewCreationError::TypeCubeNot6ArrayLayers);
        }
        // VUID-VkImageViewCreateInfo-viewType-02961
        else if view_type == ImageViewType::CubeArray && layer_count % 6 != 0 {
            return Err(ImageViewCreationError::TypeCubeArrayNotMultipleOf6ArrayLayers);
        }

        // VUID-VkImageViewCreateInfo-format-04714
        // VUID-VkImageViewCreateInfo-format-04715
        match format.ycbcr_chroma_sampling() {
            Some(ChromaSampling::Mode422) => {
                if image_inner.dimensions().width() % 2 != 0 {
                    return Err(
                        ImageViewCreationError::FormatChromaSubsamplingInvalidImageDimensions,
                    );
                }
            }
            Some(ChromaSampling::Mode420) => {
                if image_inner.dimensions().width() % 2 != 0
                    || image_inner.dimensions().height() % 2 != 0
                {
                    return Err(
                        ImageViewCreationError::FormatChromaSubsamplingInvalidImageDimensions,
                    );
                }
            }
            _ => (),
        }

        // Don't need to check features because you can't create a conversion object without the
        // feature anyway.
        if let Some(conversion) = &sampler_ycbcr_conversion {
            assert_eq!(device, conversion.device());

            // VUID-VkImageViewCreateInfo-pNext-01970
            if !component_mapping.is_identity() {
                return Err(
                    ImageViewCreationError::SamplerYcbcrConversionComponentMappingNotIdentity {
                        component_mapping,
                    },
                );
            }
        } else {
            // VUID-VkImageViewCreateInfo-format-06415
            if format.ycbcr_chroma_sampling().is_some() {
                return Err(
                    ImageViewCreationError::FormatRequiresSamplerYcbcrConversion { format },
                );
            }
        }

        Ok(format_features)
    }

    #[cfg_attr(not(feature = "document_unchecked"), doc(hidden))]
    pub unsafe fn new_unchecked(
        image: Arc<I>,
        create_info: ImageViewCreateInfo,
    ) -> Result<Arc<Self>, VulkanError> {
        let format_features =
            Self::get_format_features(create_info.format.unwrap(), image.inner().image);
        Self::new_unchecked_with_format_features(image, create_info, format_features)
    }

    unsafe fn new_unchecked_with_format_features(
        image: Arc<I>,
        create_info: ImageViewCreateInfo,
        format_features: FormatFeatures,
    ) -> Result<Arc<Self>, VulkanError> {
        let &ImageViewCreateInfo {
            view_type,
            format,
            component_mapping,
            ref subresource_range,
            mut usage,
            ref sampler_ycbcr_conversion,
            _ne: _,
        } = &create_info;

        let image_inner = image.inner().image;
        let device = image_inner.device();

        let default_usage = Self::get_default_usage(subresource_range.aspects, image_inner);

        let has_non_default_usage = if usage.is_empty() {
            usage = default_usage;
            false
        } else {
            usage == default_usage
        };

        let mut info_vk = ash::vk::ImageViewCreateInfo {
            flags: ash::vk::ImageViewCreateFlags::empty(),
            image: image_inner.internal_object(),
            view_type: view_type.into(),
            format: format.unwrap().into(),
            components: component_mapping.into(),
            subresource_range: subresource_range.clone().into(),
            ..Default::default()
        };
        let mut image_view_usage_info_vk = None;
        let mut sampler_ycbcr_conversion_info_vk = None;

        if has_non_default_usage {
            let next = image_view_usage_info_vk.insert(ash::vk::ImageViewUsageCreateInfo {
                usage: usage.into(),
                ..Default::default()
            });

            next.p_next = info_vk.p_next;
            info_vk.p_next = next as *const _ as *const _;
        }

        if let Some(conversion) = sampler_ycbcr_conversion {
            let next =
                sampler_ycbcr_conversion_info_vk.insert(ash::vk::SamplerYcbcrConversionInfo {
                    conversion: conversion.internal_object(),
                    ..Default::default()
                });

            next.p_next = info_vk.p_next;
            info_vk.p_next = next as *const _ as *const _;
        }

        let handle = {
            let fns = device.fns();
            let mut output = MaybeUninit::uninit();
            (fns.v1_0.create_image_view)(
                device.internal_object(),
                &info_vk,
                ptr::null(),
                output.as_mut_ptr(),
            )
            .result()
            .map_err(VulkanError::from)?;
            output.assume_init()
        };

        Self::from_handle_with_format_features(image, handle, create_info, format_features)
    }

    /// Creates a default `ImageView`. Equivalent to
    /// `ImageView::new(image, ImageViewCreateInfo::from_image(image))`.
    pub fn new_default(image: Arc<I>) -> Result<Arc<ImageView<I>>, ImageViewCreationError> {
        let create_info = ImageViewCreateInfo::from_image(&image);
        Self::new(image, create_info)
    }

    /// Creates a new `ImageView` from a raw object handle.
    ///
    /// # Safety
    ///
    /// - `handle` must be a valid Vulkan object handle created from `image`.
    /// - `create_info` must match the info used to create the object.
    pub unsafe fn from_handle(
        image: Arc<I>,
        handle: ash::vk::ImageView,
        create_info: ImageViewCreateInfo,
    ) -> Result<Arc<Self>, VulkanError> {
        let format_features =
            Self::get_format_features(create_info.format.unwrap(), image.inner().image);
        Self::from_handle_with_format_features(image, handle, create_info, format_features)
    }

    unsafe fn from_handle_with_format_features(
        image: Arc<I>,
        handle: ash::vk::ImageView,
        create_info: ImageViewCreateInfo,
        format_features: FormatFeatures,
    ) -> Result<Arc<Self>, VulkanError> {
        let ImageViewCreateInfo {
            view_type,
            format,
            component_mapping,
            subresource_range,
            mut usage,
            sampler_ycbcr_conversion,
            _ne: _,
        } = create_info;

        let image_inner = image.inner().image;
        let device = image_inner.device();

        if usage.is_empty() {
            usage = Self::get_default_usage(subresource_range.aspects, image_inner);
        }

        let mut filter_cubic = false;
        let mut filter_cubic_minmax = false;

        if device
            .physical_device()
            .supported_extensions()
            .ext_filter_cubic
        {
            // Use unchecked, because all validation has been done above or is validated by the
            // image.
            let properties =
                device
                    .physical_device()
                    .image_format_properties_unchecked(ImageFormatInfo {
                        format: image_inner.format(),
                        image_type: image.dimensions().image_type(),
                        tiling: image_inner.tiling(),
                        usage: *image_inner.usage(),
                        image_view_type: Some(view_type),
                        mutable_format: image_inner.mutable_format(),
                        cube_compatible: image_inner.cube_compatible(),
                        array_2d_compatible: image_inner.array_2d_compatible(),
                        block_texel_view_compatible: image_inner.block_texel_view_compatible(),
                        ..Default::default()
                    })?;

            if let Some(properties) = properties {
                filter_cubic = properties.filter_cubic;
                filter_cubic_minmax = properties.filter_cubic_minmax;
            }
        }

        Ok(Arc::new(ImageView {
            handle,
            image,

            view_type,
            format,
            format_features,
            component_mapping,
            subresource_range,
            usage,
            sampler_ycbcr_conversion,

            filter_cubic,
            filter_cubic_minmax,
        }))
    }

    // https://www.khronos.org/registry/vulkan/specs/1.3-extensions/man/html/VkImageViewCreateInfo.html#_description
    fn get_default_usage(aspects: ImageAspects, image: &UnsafeImage) -> ImageUsage {
        let has_stencil_aspect = aspects.stencil;
        let has_non_stencil_aspect = !(ImageAspects {
            stencil: false,
            ..aspects
        })
        .is_empty();

        if has_stencil_aspect && has_non_stencil_aspect {
            *image.usage() & *image.stencil_usage()
        } else if has_stencil_aspect {
            *image.stencil_usage()
        } else if has_non_stencil_aspect {
            *image.usage()
        } else {
            unreachable!()
        }
    }

    // https://www.khronos.org/registry/vulkan/specs/1.3-extensions/html/chap12.html#resources-image-view-format-features
    unsafe fn get_format_features(format: Format, image: &UnsafeImage) -> FormatFeatures {
        let device = image.device();

        let format_features = if Some(format) != image.format() {
            // Use unchecked, because all validation should have been done before calling.
            let format_properties = device.physical_device().format_properties_unchecked(format);

            match image.tiling() {
                ImageTiling::Optimal => format_properties.optimal_tiling_features,
                ImageTiling::Linear => format_properties.linear_tiling_features,
            }
        } else {
            *image.format_features()
        };

        if device.enabled_extensions().khr_format_feature_flags2 {
            format_features
        } else {
            let is_without_format = format.shader_storage_image_without_format();

            FormatFeatures {
                sampled_image_depth_comparison: format.type_color().is_none()
                    && format_features.sampled_image,
                storage_read_without_format: is_without_format
                    && device
                        .enabled_features()
                        .shader_storage_image_read_without_format,
                storage_write_without_format: is_without_format
                    && device
                        .enabled_features()
                        .shader_storage_image_write_without_format,
                ..format_features
            }
        }
    }

    /// Returns the wrapped image that this image view was created from.
    pub fn image(&self) -> &Arc<I> {
        &self.image
    }
}

impl<I> Drop for ImageView<I>
where
    I: ImageAccess + ?Sized,
{
    fn drop(&mut self) {
        unsafe {
            let device = self.device();
            let fns = device.fns();
            (fns.v1_0.destroy_image_view)(device.internal_object(), self.handle, ptr::null());
        }
    }
}

unsafe impl<I> VulkanObject for ImageView<I>
where
    I: ImageAccess + ?Sized,
{
    type Object = ash::vk::ImageView;

    fn internal_object(&self) -> ash::vk::ImageView {
        self.handle
    }
}

unsafe impl<I> DeviceOwned for ImageView<I>
where
    I: ImageAccess + ?Sized,
{
    fn device(&self) -> &Arc<Device> {
        self.image.inner().image.device()
    }
}

impl<I> PartialEq for ImageView<I>
where
    I: ImageAccess + ?Sized,
{
    fn eq(&self, other: &Self) -> bool {
        self.handle == other.handle && self.device() == other.device()
    }
}

impl<I> Eq for ImageView<I> where I: ImageAccess + ?Sized {}

impl<I> Hash for ImageView<I>
where
    I: ImageAccess + ?Sized,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.handle.hash(state);
        self.device().hash(state);
    }
}

/// Parameters to create a new `ImageView`.
#[derive(Debug)]
pub struct ImageViewCreateInfo {
    /// The image view type.
    ///
    /// The view type must be compatible with the dimensions of the image and the selected array
    /// layers.
    ///
    /// The default value is [`ImageViewType::Dim2d`].
    pub view_type: ImageViewType,

    /// The format of the image view.
    ///
    /// If this is set to a format that is different from the image, the image must be created with
    /// the `mutable_format` flag.
    ///
    /// The default value is `None`, which must be overridden.
    pub format: Option<Format>,

    /// How to map components of each pixel.
    ///
    /// The default value is [`ComponentMapping::identity()`].
    pub component_mapping: ComponentMapping,

    /// The subresource range of the image that the view should cover.
    ///
    /// The default value is empty, which must be overridden.
    pub subresource_range: ImageSubresourceRange,

    /// How the image view is going to be used.
    ///
    /// If `usage` is empty, then a default value is used based on the parent image's usages.
    /// Depending on the image aspects selected in `subresource_range`,
    /// the default `usage` will be equal to the parent image's `usage`, its `stencil_usage`,
    /// or the intersection of the two.
    ///
    /// If you set `usage` to a different value from the default, then the device API version must
    /// be at least 1.1, or the [`khr_maintenance2`](crate::device::DeviceExtensions::khr_maintenance2)
    /// extension must be enabled on the device. The specified `usage` must be a subset of the
    /// default value; usages that are not set for the parent image are not allowed.
    ///
    /// The default value is [`ImageUsage::empty()`].
    pub usage: ImageUsage,

    /// The sampler YCbCr conversion to be used with the image view.
    ///
    /// If set to `Some`, several restrictions apply:
    /// - The `component_mapping` must be the identity swizzle for all components.
    /// - If the image view is to be used in a shader, it must be in a combined image sampler
    ///   descriptor, a separate sampled image descriptor is not allowed.
    /// - The corresponding sampler must have the same sampler YCbCr object or an identically
    ///   created one, and must be used as an immutable sampler within a descriptor set layout.
    ///
    /// The default value is `None`.
    pub sampler_ycbcr_conversion: Option<Arc<SamplerYcbcrConversion>>,

    pub _ne: crate::NonExhaustive,
}

impl Default for ImageViewCreateInfo {
    #[inline]
    fn default() -> Self {
        Self {
            view_type: ImageViewType::Dim2d,
            format: None,
            component_mapping: ComponentMapping::identity(),
            subresource_range: ImageSubresourceRange {
                aspects: ImageAspects::empty(),
                array_layers: 0..0,
                mip_levels: 0..0,
            },
            usage: ImageUsage::empty(),
            sampler_ycbcr_conversion: None,
            _ne: crate::NonExhaustive(()),
        }
    }
}

impl ImageViewCreateInfo {
    /// Returns an `ImageViewCreateInfo` with the `view_type` determined from the image type and
    /// array layers, and `subresource_range` determined from the image format and covering the
    /// whole image.
    pub fn from_image(image: &(impl ImageAccess + ?Sized)) -> Self {
        Self {
            view_type: match image.dimensions() {
                ImageDimensions::Dim1d {
                    array_layers: 1, ..
                } => ImageViewType::Dim1d,
                ImageDimensions::Dim1d { .. } => ImageViewType::Dim1dArray,
                ImageDimensions::Dim2d {
                    array_layers: 1, ..
                } => ImageViewType::Dim2d,
                ImageDimensions::Dim2d { .. } => ImageViewType::Dim2dArray,
                ImageDimensions::Dim3d { .. } => ImageViewType::Dim3d,
            },
            format: Some(image.format()),
            subresource_range: image.subresource_range(),
            ..Default::default()
        }
    }
}

/// Error that can happen when creating an image view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageViewCreationError {
    /// Allocating memory failed.
    OomError(OomError),

    RequirementNotMet {
        required_for: &'static str,
        requires_one_of: RequiresOneOf,
    },

    /// A 2D image view was requested from a 3D image, but a range of multiple mip levels was
    /// specified.
    Array2dCompatibleMultipleMipLevels,

    /// The specified range of array layers was not a subset of those in the image.
    ArrayLayersOutOfRange { range_end: u32, max: u32 },

    /// The image has the `block_texel_view_compatible` flag, but a range of multiple array layers
    /// was specified.
    BlockTexelViewCompatibleMultipleArrayLayers,

    /// The image has the `block_texel_view_compatible` flag, but a range of multiple mip levels
    /// was specified.
    BlockTexelViewCompatibleMultipleMipLevels,

    /// The image has the `block_texel_view_compatible` flag, and an uncompressed format was
    /// requested, and the image view type was `Dim3d`.
    BlockTexelViewCompatibleUncompressedIs3d,

    /// The requested format has chroma subsampling, but the width and/or height of the image was
    /// not a multiple of 2.
    FormatChromaSubsamplingInvalidImageDimensions,

    /// The requested format was not compatible with the image.
    FormatNotCompatible,

    /// The given format was not supported by the device.
    FormatNotSupported,

    /// The format requires a sampler YCbCr conversion, but none was provided.
    FormatRequiresSamplerYcbcrConversion { format: Format },

    /// A requested usage flag was not supported by the given format.
    FormatUsageNotSupported { usage: &'static str },

    /// An aspect was selected that was not present in the image.
    ImageAspectsNotCompatible {
        aspects: ImageAspects,
        image_aspects: ImageAspects,
    },

    /// The image was not created with
    /// [one of the required usages](https://registry.khronos.org/vulkan/specs/1.2-extensions/html/vkspec.html#valid-imageview-imageusage)
    /// for image views.
    ImageMissingUsage,

    /// A 2D image view was requested from a 3D image, but the image was not created with the
    /// `array_2d_compatible` flag.
    ImageNotArray2dCompatible,

    /// A cube image view type was requested, but the image was not created with the
    /// `cube_compatible` flag.
    ImageNotCubeCompatible,

    /// The given image view type was not compatible with the type of the image.
    ImageTypeNotCompatible,

    /// The requested [`ImageViewType`] was not compatible with the image, or with the specified
    /// ranges of array layers and mipmap levels.
    IncompatibleType,

    /// The specified range of mip levels was not a subset of those in the image.
    MipLevelsOutOfRange { range_end: u32, max: u32 },

    /// The image has multisampling enabled, but the image view type was not `Dim2d` or
    /// `Dim2dArray`.
    MultisamplingNot2d,

    /// Sampler YCbCr conversion was enabled, but `component_mapping` was not the identity mapping.
    SamplerYcbcrConversionComponentMappingNotIdentity { component_mapping: ComponentMapping },

    /// The `CubeArray` image view type was specified, but the range of array layers did not have a
    /// size that is a multiple 6.
    TypeCubeArrayNotMultipleOf6ArrayLayers,

    /// The `Cube` image view type was specified, but the range of array layers did not have a size
    /// of 6.
    TypeCubeNot6ArrayLayers,

    /// A non-arrayed image view type was specified, but a range of multiple array layers was
    /// specified.
    TypeNonArrayedMultipleArrayLayers,

    /// The provided `usage` is not supported by the parent image.
    UsageNotSupportedByImage {
        usage: ImageUsage,
        supported_usage: ImageUsage,
    },
}

impl Error for ImageViewCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ImageViewCreationError::OomError(err) => Some(err),
            _ => None,
        }
    }
}

impl Display for ImageViewCreationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self {
            Self::OomError(_) => write!(f, "allocating memory failed",),
            Self::RequirementNotMet {
                required_for,
                requires_one_of,
            } => write!(
                f,
                "a requirement was not met for: {}; requires one of: {}",
                required_for, requires_one_of,
            ),
            Self::Array2dCompatibleMultipleMipLevels => write!(
                f,
                "a 2D image view was requested from a 3D image, but a range of multiple mip levels \
                was specified",
            ),
            Self::ArrayLayersOutOfRange { .. } => write!(
                f,
                "the specified range of array layers was not a subset of those in the image",
            ),
            Self::BlockTexelViewCompatibleMultipleArrayLayers => write!(
                f,
                "the image has the `block_texel_view_compatible` flag, but a range of multiple \
                array layers was specified",
            ),
            Self::BlockTexelViewCompatibleMultipleMipLevels => write!(
                f,
                "the image has the `block_texel_view_compatible` flag, but a range of multiple mip \
                levels was specified",
            ),
            Self::BlockTexelViewCompatibleUncompressedIs3d => write!(
                f,
                "the image has the `block_texel_view_compatible` flag, and an uncompressed format \
                was requested, and the image view type was `Dim3d`",
            ),
            Self::FormatChromaSubsamplingInvalidImageDimensions => write!(
                f,
                "the requested format has chroma subsampling, but the width and/or height of the \
                image was not a multiple of 2",
            ),
            Self::FormatNotCompatible => {
                write!(f, "the requested format was not compatible with the image")
            }
            Self::FormatNotSupported => {
                write!(f, "the given format was not supported by the device")
            }
            Self::FormatRequiresSamplerYcbcrConversion { .. } => write!(
                f,
                "the format requires a sampler YCbCr conversion, but none was provided",
            ),
            Self::FormatUsageNotSupported { .. } => write!(
                f,
                "a requested usage flag was not supported by the given format",
            ),
            Self::ImageAspectsNotCompatible { .. } => write!(
                f,
                "an aspect was selected that was not present in the image",
            ),
            Self::ImageMissingUsage => write!(
                f,
                "the image was not created with one of the required usages for image views",
            ),
            Self::ImageNotArray2dCompatible => write!(
                f,
                "a 2D image view was requested from a 3D image, but the image was not created with \
                the `array_2d_compatible` flag",
            ),
            Self::ImageNotCubeCompatible => write!(
                f,
                "a cube image view type was requested, but the image was not created with the \
                `cube_compatible` flag",
            ),
            Self::ImageTypeNotCompatible => write!(
                f,
                "the given image view type was not compatible with the type of the image",
            ),
            Self::IncompatibleType => write!(
                f,
                "image view type is not compatible with image, array layers or mipmap levels",
            ),
            Self::MipLevelsOutOfRange { .. } => write!(
                f,
                "the specified range of mip levels was not a subset of those in the image",
            ),
            Self::MultisamplingNot2d => write!(
                f,
                "the image has multisampling enabled, but the image view type was not `Dim2d` or \
                `Dim2dArray`",
            ),
            Self::SamplerYcbcrConversionComponentMappingNotIdentity { .. } => write!(
                f,
                "sampler YCbCr conversion was enabled, but `component_mapping` was not the \
                identity mapping",
            ),
            Self::TypeCubeArrayNotMultipleOf6ArrayLayers => write!(
                f,
                "the `CubeArray` image view type was specified, but the range of array layers did \
                not have a size that is a multiple 6",
            ),
            Self::TypeCubeNot6ArrayLayers => write!(
                f,
                "the `Cube` image view type was specified, but the range of array layers did not \
                have a size of 6",
            ),
            Self::TypeNonArrayedMultipleArrayLayers => write!(
                f,
                "a non-arrayed image view type was specified, but a range of multiple array layers \
                was specified",
            ),
            Self::UsageNotSupportedByImage {
                usage: _,
                supported_usage: _,
            } => write!(
                f,
                "the provided `usage` is not supported by the parent image",
            ),
        }
    }
}

impl From<OomError> for ImageViewCreationError {
    fn from(err: OomError) -> ImageViewCreationError {
        ImageViewCreationError::OomError(err)
    }
}

impl From<VulkanError> for ImageViewCreationError {
    fn from(err: VulkanError) -> ImageViewCreationError {
        match err {
            err @ VulkanError::OutOfHostMemory => OomError::from(err).into(),
            err @ VulkanError::OutOfDeviceMemory => OomError::from(err).into(),
            _ => panic!("unexpected error: {:?}", err),
        }
    }
}

impl From<RequirementNotMet> for ImageViewCreationError {
    fn from(err: RequirementNotMet) -> Self {
        Self::RequirementNotMet {
            required_for: err.required_for,
            requires_one_of: err.requires_one_of,
        }
    }
}

vulkan_enum! {
    /// The geometry type of an image view.
    #[non_exhaustive]
    ImageViewType = ImageViewType(i32);

    // TODO: document
    Dim1d = TYPE_1D,

    // TODO: document
    Dim2d = TYPE_2D,

    // TODO: document
    Dim3d = TYPE_3D,

    // TODO: document
    Cube = CUBE,

    // TODO: document
    Dim1dArray = TYPE_1D_ARRAY,

    // TODO: document
    Dim2dArray = TYPE_2D_ARRAY,

    // TODO: document
    CubeArray = CUBE_ARRAY,
}

impl ImageViewType {
    /// Returns whether the type is arrayed.
    #[inline]
    pub fn is_arrayed(&self) -> bool {
        match self {
            Self::Dim1d | Self::Dim2d | Self::Dim3d | Self::Cube => false,
            Self::Dim1dArray | Self::Dim2dArray | Self::CubeArray => true,
        }
    }

    /// Returns whether `self` is compatible with the given `image_type`.
    #[inline]
    pub fn is_compatible_with(&self, image_type: ImageType) -> bool {
        matches!(
            (*self, image_type,),
            (
                ImageViewType::Dim1d | ImageViewType::Dim1dArray,
                ImageType::Dim1d
            ) | (
                ImageViewType::Dim2d | ImageViewType::Dim2dArray,
                ImageType::Dim2d | ImageType::Dim3d
            ) | (
                ImageViewType::Cube | ImageViewType::CubeArray,
                ImageType::Dim2d
            ) | (ImageViewType::Dim3d, ImageType::Dim3d)
        )
    }
}

/// Trait for types that represent the GPU can access an image view.
pub unsafe trait ImageViewAbstract:
    VulkanObject<Object = ash::vk::ImageView> + DeviceOwned + Debug + Send + Sync
{
    /// Returns the wrapped image that this image view was created from.
    fn image(&self) -> Arc<dyn ImageAccess>;

    /// Returns the component mapping of this view.
    fn component_mapping(&self) -> ComponentMapping;

    /// Returns the dimensions of this view.
    #[inline]
    fn dimensions(&self) -> ImageDimensions {
        let subresource_range = self.subresource_range();
        let array_layers =
            subresource_range.array_layers.end - subresource_range.array_layers.start;

        match self.image().dimensions() {
            ImageDimensions::Dim1d { width, .. } => ImageDimensions::Dim1d {
                width,
                array_layers,
            },
            ImageDimensions::Dim2d { width, height, .. } => ImageDimensions::Dim2d {
                width,
                height,
                array_layers,
            },
            ImageDimensions::Dim3d {
                width,
                height,
                depth,
            } => ImageDimensions::Dim3d {
                width,
                height,
                depth,
            },
        }
    }

    /// Returns whether the image view supports sampling with a
    /// [`Cubic`](crate::sampler::Filter::Cubic) `mag_filter` or `min_filter`.
    fn filter_cubic(&self) -> bool;

    /// Returns whether the image view supports sampling with a
    /// [`Cubic`](crate::sampler::Filter::Cubic) `mag_filter` or `min_filter`, and with a
    /// [`Min`](crate::sampler::SamplerReductionMode::Min) or
    /// [`Max`](crate::sampler::SamplerReductionMode::Max) `reduction_mode`.
    fn filter_cubic_minmax(&self) -> bool;

    /// Returns the format of this view. This can be different from the parent's format.
    fn format(&self) -> Option<Format>;

    /// Returns the features supported by the image view's format.
    fn format_features(&self) -> &FormatFeatures;

    /// Returns the sampler YCbCr conversion that this image view was created with, if any.
    fn sampler_ycbcr_conversion(&self) -> Option<&Arc<SamplerYcbcrConversion>>;

    /// Returns the subresource range of the wrapped image that this view exposes.
    fn subresource_range(&self) -> &ImageSubresourceRange;

    /// Returns the usage of the image view.
    fn usage(&self) -> &ImageUsage;

    /// Returns the [`ImageViewType`] of this image view.
    fn view_type(&self) -> ImageViewType;
}

unsafe impl<I> ImageViewAbstract for ImageView<I>
where
    I: ImageAccess + Debug + 'static,
{
    fn image(&self) -> Arc<dyn ImageAccess> {
        self.image.clone()
    }

    fn component_mapping(&self) -> ComponentMapping {
        self.component_mapping
    }

    fn filter_cubic(&self) -> bool {
        self.filter_cubic
    }

    fn filter_cubic_minmax(&self) -> bool {
        self.filter_cubic_minmax
    }

    fn format(&self) -> Option<Format> {
        self.format
    }

    fn format_features(&self) -> &FormatFeatures {
        &self.format_features
    }

    fn sampler_ycbcr_conversion(&self) -> Option<&Arc<SamplerYcbcrConversion>> {
        self.sampler_ycbcr_conversion.as_ref()
    }

    fn subresource_range(&self) -> &ImageSubresourceRange {
        &self.subresource_range
    }

    fn usage(&self) -> &ImageUsage {
        &self.usage
    }

    fn view_type(&self) -> ImageViewType {
        self.view_type
    }
}

unsafe impl ImageViewAbstract for ImageView<dyn ImageAccess> {
    #[inline]
    fn image(&self) -> Arc<dyn ImageAccess> {
        self.image.clone()
    }

    #[inline]
    fn component_mapping(&self) -> ComponentMapping {
        self.component_mapping
    }

    #[inline]
    fn filter_cubic(&self) -> bool {
        self.filter_cubic
    }

    #[inline]
    fn filter_cubic_minmax(&self) -> bool {
        self.filter_cubic_minmax
    }

    #[inline]
    fn format(&self) -> Option<Format> {
        self.format
    }

    #[inline]
    fn format_features(&self) -> &FormatFeatures {
        &self.format_features
    }

    #[inline]
    fn sampler_ycbcr_conversion(&self) -> Option<&Arc<SamplerYcbcrConversion>> {
        self.sampler_ycbcr_conversion.as_ref()
    }

    #[inline]
    fn subresource_range(&self) -> &ImageSubresourceRange {
        &self.subresource_range
    }

    #[inline]
    fn usage(&self) -> &ImageUsage {
        &self.usage
    }

    #[inline]
    fn view_type(&self) -> ImageViewType {
        self.view_type
    }
}

impl PartialEq for dyn ImageViewAbstract {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.internal_object() == other.internal_object() && self.device() == other.device()
    }
}

impl Eq for dyn ImageViewAbstract {}

impl Hash for dyn ImageViewAbstract {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.internal_object().hash(state);
        self.device().hash(state);
    }
}
