// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use super::{
    sys::UnsafeImage, traits::ImageContent, ImageAccess, ImageCreateFlags, ImageCreationError,
    ImageDescriptorLayouts, ImageDimensions, ImageInner, ImageLayout, ImageUsage,
};
use crate::{
    device::{Device, DeviceOwned, Queue},
    format::Format,
    image::{sys::UnsafeImageCreateInfo, view::ImageView},
    memory::{
        pool::{
            alloc_dedicated_with_exportable_fd, AllocFromRequirementsFilter, AllocLayout,
            MappingRequirement, MemoryPoolAlloc, PotentialDedicatedAllocation, StandardMemoryPool,
        },
        DedicatedAllocation, DeviceMemoryError, ExternalMemoryHandleType,
        ExternalMemoryHandleTypes, MemoryPool,
    },
    sync::Sharing,
    DeviceSize,
};
use smallvec::SmallVec;
use std::{
    fs::File,
    hash::{Hash, Hasher},
    sync::Arc,
};

/// General-purpose image in device memory. Can be used for any usage, but will be slower than a
/// specialized image.
#[derive(Debug)]
pub struct StorageImage<A = Arc<StandardMemoryPool>>
where
    A: MemoryPool,
{
    // Inner implementation.
    image: Arc<UnsafeImage>,

    // Memory used to back the image.
    memory: PotentialDedicatedAllocation<A::Alloc>,

    // Dimensions of the image.
    dimensions: ImageDimensions,
}

impl StorageImage {
    /// Creates a new image with the given dimensions and format.
    pub fn new(
        device: Arc<Device>,
        dimensions: ImageDimensions,
        format: Format,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<StorageImage>, ImageCreationError> {
        let aspects = format.aspects();
        let is_depth = aspects.depth || aspects.stencil;

        if format.compression().is_some() {
            panic!() // TODO: message?
        }

        let usage = ImageUsage {
            transfer_src: true,
            transfer_dst: true,
            sampled: true,
            storage: true,
            color_attachment: !is_depth,
            depth_stencil_attachment: is_depth,
            input_attachment: true,
            ..ImageUsage::empty()
        };
        let flags = ImageCreateFlags::empty();

        StorageImage::with_usage(
            device,
            dimensions,
            format,
            usage,
            flags,
            queue_family_indices,
        )
    }

    /// Same as `new`, but allows specifying the usage.
    pub fn with_usage(
        device: Arc<Device>,
        dimensions: ImageDimensions,
        format: Format,
        usage: ImageUsage,
        flags: ImageCreateFlags,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<StorageImage>, ImageCreationError> {
        let queue_family_indices: SmallVec<[_; 4]> = queue_family_indices.into_iter().collect();

        let image = UnsafeImage::new(
            device.clone(),
            UnsafeImageCreateInfo {
                dimensions,
                format: Some(format),
                usage,
                sharing: if queue_family_indices.len() >= 2 {
                    Sharing::Concurrent(queue_family_indices)
                } else {
                    Sharing::Exclusive
                },
                mutable_format: flags.mutable_format,
                cube_compatible: flags.cube_compatible,
                array_2d_compatible: flags.array_2d_compatible,
                block_texel_view_compatible: flags.block_texel_view_compatible,
                ..Default::default()
            },
        )?;

        let mem_reqs = image.memory_requirements();
        let memory = MemoryPool::alloc_from_requirements(
            &device.standard_memory_pool(),
            &mem_reqs,
            AllocLayout::Optimal,
            MappingRequirement::DoNotMap,
            Some(DedicatedAllocation::Image(&image)),
            |t| {
                if t.property_flags.device_local {
                    AllocFromRequirementsFilter::Preferred
                } else {
                    AllocFromRequirementsFilter::Allowed
                }
            },
        )?;
        debug_assert!((memory.offset() % mem_reqs.alignment) == 0);
        unsafe {
            image.bind_memory(memory.memory(), memory.offset())?;
        }

        Ok(Arc::new(StorageImage {
            image,
            memory,
            dimensions,
        }))
    }

    pub fn new_with_exportable_fd(
        device: Arc<Device>,
        dimensions: ImageDimensions,
        format: Format,
        usage: ImageUsage,
        flags: ImageCreateFlags,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<StorageImage>, ImageCreationError> {
        let queue_family_indices: SmallVec<[_; 4]> = queue_family_indices.into_iter().collect();

        let image = UnsafeImage::new(
            device.clone(),
            UnsafeImageCreateInfo {
                dimensions,
                format: Some(format),
                usage,
                sharing: if queue_family_indices.len() >= 2 {
                    Sharing::Concurrent(queue_family_indices)
                } else {
                    Sharing::Exclusive
                },
                external_memory_handle_types: ExternalMemoryHandleTypes {
                    opaque_fd: true,
                    ..ExternalMemoryHandleTypes::empty()
                },
                mutable_format: flags.mutable_format,
                cube_compatible: flags.cube_compatible,
                array_2d_compatible: flags.array_2d_compatible,
                block_texel_view_compatible: flags.block_texel_view_compatible,
                ..Default::default()
            },
        )?;

        let mem_reqs = image.memory_requirements();
        let memory = alloc_dedicated_with_exportable_fd(
            device,
            &mem_reqs,
            AllocLayout::Optimal,
            MappingRequirement::DoNotMap,
            DedicatedAllocation::Image(&image),
            |t| {
                if t.property_flags.device_local {
                    AllocFromRequirementsFilter::Preferred
                } else {
                    AllocFromRequirementsFilter::Allowed
                }
            },
        )?;
        debug_assert!((memory.offset() % mem_reqs.alignment) == 0);
        unsafe {
            image.bind_memory(memory.memory(), memory.offset())?;
        }

        Ok(Arc::new(StorageImage {
            image,
            memory,
            dimensions,
        }))
    }

    /// Allows the creation of a simple 2D general purpose image view from `StorageImage`.
    #[inline]
    pub fn general_purpose_image_view(
        queue: Arc<Queue>,
        size: [u32; 2],
        format: Format,
        usage: ImageUsage,
    ) -> Result<Arc<ImageView<StorageImage>>, ImageCreationError> {
        let dims = ImageDimensions::Dim2d {
            width: size[0],
            height: size[1],
            array_layers: 1,
        };
        let flags = ImageCreateFlags::empty();
        let image_result = StorageImage::with_usage(
            queue.device().clone(),
            dims,
            format,
            usage,
            flags,
            Some(queue.queue_family_index()),
        );

        match image_result {
            Ok(image) => {
                let image_view = ImageView::new_default(image);
                match image_view {
                    Ok(view) => Ok(view),
                    Err(e) => Err(ImageCreationError::DirectImageViewCreationFailed(e)),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Exports posix file descriptor for the allocated memory.
    /// Requires `khr_external_memory_fd` and `khr_external_memory` extensions to be loaded.
    #[inline]
    pub fn export_posix_fd(&self) -> Result<File, DeviceMemoryError> {
        self.memory
            .memory()
            .export_fd(ExternalMemoryHandleType::OpaqueFd)
    }

    /// Return the size of the allocated memory (used e.g. with cuda).
    #[inline]
    pub fn mem_size(&self) -> DeviceSize {
        self.memory.memory().allocation_size()
    }
}

unsafe impl<A> DeviceOwned for StorageImage<A>
where
    A: MemoryPool,
{
    fn device(&self) -> &Arc<Device> {
        self.image.device()
    }
}

unsafe impl<A> ImageAccess for StorageImage<A>
where
    A: MemoryPool,
{
    fn inner(&self) -> ImageInner<'_> {
        ImageInner {
            image: &self.image,
            first_layer: 0,
            num_layers: self.dimensions.array_layers(),
            first_mipmap_level: 0,
            num_mipmap_levels: 1,
        }
    }

    fn initial_layout_requirement(&self) -> ImageLayout {
        ImageLayout::General
    }

    fn final_layout_requirement(&self) -> ImageLayout {
        ImageLayout::General
    }

    fn descriptor_layouts(&self) -> Option<ImageDescriptorLayouts> {
        Some(ImageDescriptorLayouts {
            storage_image: ImageLayout::General,
            combined_image_sampler: ImageLayout::General,
            sampled_image: ImageLayout::General,
            input_attachment: ImageLayout::General,
        })
    }
}

unsafe impl<P, A> ImageContent<P> for StorageImage<A>
where
    A: MemoryPool,
{
    fn matches_format(&self) -> bool {
        true // FIXME:
    }
}

impl<A> PartialEq for StorageImage<A>
where
    A: MemoryPool,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner() == other.inner()
    }
}

impl<A> Eq for StorageImage<A> where A: MemoryPool {}

impl<A> Hash for StorageImage<A>
where
    A: MemoryPool,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::StorageImage;
    use crate::{
        format::Format,
        image::{
            view::ImageViewCreationError, ImageAccess, ImageCreationError, ImageDimensions,
            ImageUsage,
        },
    };

    #[test]
    fn create() {
        let (device, queue) = gfx_dev_and_queue!();
        let _img = StorageImage::new(
            device,
            ImageDimensions::Dim2d {
                width: 32,
                height: 32,
                array_layers: 1,
            },
            Format::R8G8B8A8_UNORM,
            Some(queue.queue_family_index()),
        )
        .unwrap();
    }

    #[test]
    fn create_general_purpose_image_view() {
        let (_device, queue) = gfx_dev_and_queue!();
        let usage = ImageUsage {
            transfer_src: true,
            transfer_dst: true,
            color_attachment: true,
            ..ImageUsage::empty()
        };
        let img_view = StorageImage::general_purpose_image_view(
            queue,
            [32, 32],
            Format::R8G8B8A8_UNORM,
            usage,
        )
        .unwrap();
        assert_eq!(img_view.image().usage(), &usage);
    }

    #[test]
    fn create_general_purpose_image_view_failed() {
        let (_device, queue) = gfx_dev_and_queue!();
        // Not valid for image view...
        let usage = ImageUsage {
            transfer_src: true,
            ..ImageUsage::empty()
        };
        let img_result = StorageImage::general_purpose_image_view(
            queue,
            [32, 32],
            Format::R8G8B8A8_UNORM,
            usage,
        );
        assert_eq!(
            img_result,
            Err(ImageCreationError::DirectImageViewCreationFailed(
                ImageViewCreationError::ImageMissingUsage
            ))
        );
    }
}
