// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! Buffer whose content is read-written by the GPU only.
//!
//! Each access from the CPU or from the GPU locks the whole buffer for either reading or writing.
//! You can read the buffer multiple times simultaneously from multiple queues. Trying to read and
//! write simultaneously, or write and write simultaneously will block with a semaphore.
//!

use super::{
    sys::{UnsafeBuffer, UnsafeBufferCreateInfo},
    BufferAccess, BufferAccessObject, BufferContents, BufferCreationError, BufferInner,
    BufferUsage, CpuAccessibleBuffer, TypedBufferAccess,
};
use crate::{
    command_buffer::{
        allocator::CommandBufferAllocator, AutoCommandBufferBuilder, CommandBufferBeginError,
        CopyBufferInfo,
    },
    device::{Device, DeviceOwned},
    memory::{
        pool::{
            alloc_dedicated_with_exportable_fd, AllocFromRequirementsFilter, AllocLayout,
            MappingRequirement, MemoryPoolAlloc, PotentialDedicatedAllocation,
            StandardMemoryPoolAlloc,
        },
        DedicatedAllocation, DeviceMemoryError, ExternalMemoryHandleType, MemoryPool,
        MemoryRequirements,
    },
    sync::Sharing,
    DeviceSize,
};
use smallvec::SmallVec;
use std::{
    error::Error,
    fmt::{Display, Error as FmtError, Formatter},
    fs::File,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::size_of,
    sync::Arc,
};

/// Buffer whose content is in device-local memory.
///
/// This buffer type is useful in order to store intermediary data. For example you execute a
/// compute shader that writes to this buffer, then read the content of the buffer in a following
/// compute or graphics pipeline.
///
/// The `DeviceLocalBuffer` will be in device-local memory, unless the device doesn't provide any
/// device-local memory.
///
/// # Usage
///
/// Since a `DeviceLocalBuffer` can only be directly accessed by the GPU, data cannot be transfered
/// between the host process and the buffer alone. One must use additional buffers which are
/// accessible to the CPU as staging areas, then use command buffers to execute the necessary data
/// transfers.
///
/// Despite this, if one knows in advance that a buffer will not need to be frequently accessed by
/// the host, then there may be significant performance gains by using a `DeviceLocalBuffer` over a
/// buffer type which allows host access.
///
/// # Examples
///
/// The following example outlines the general strategy one may take when initializing a
/// `DeviceLocalBuffer`.
///
/// ```
/// use vulkano::buffer::{BufferUsage, CpuAccessibleBuffer, DeviceLocalBuffer};
/// use vulkano::command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage, CopyBufferInfo, PrimaryCommandBuffer};
/// use vulkano::sync::GpuFuture;
/// # let device: std::sync::Arc<vulkano::device::Device> = return;
/// # let queue: std::sync::Arc<vulkano::device::Queue> = return;
/// # let command_buffer_allocator: vulkano::command_buffer::allocator::StandardCommandBufferAllocator = return;
///
/// // Simple iterator to construct test data.
/// let data = (0..10_000).map(|i| i as f32);
///
/// // Create a CPU accessible buffer initialized with the data.
/// let temporary_accessible_buffer = CpuAccessibleBuffer::from_iter(
///     device.clone(),
///     BufferUsage { transfer_src: true, ..BufferUsage::empty() }, // Specify this buffer will be used as a transfer source.
///     false,
///     data,
/// )
/// .unwrap();
///
/// // Create a buffer array on the GPU with enough space for `10_000` floats.
/// let device_local_buffer = DeviceLocalBuffer::<[f32]>::array(
///     device.clone(),
///     10_000 as vulkano::DeviceSize,
///     BufferUsage {
///         storage_buffer: true,
///         transfer_dst: true,
///         ..BufferUsage::empty()
///     }, // Specify use as a storage buffer and transfer destination.
///     device.active_queue_family_indices().iter().copied(),
/// )
/// .unwrap();
///
/// // Create a one-time command to copy between the buffers.
/// let mut cbb = AutoCommandBufferBuilder::primary(
///     &command_buffer_allocator,
///     queue.queue_family_index(),
///     CommandBufferUsage::OneTimeSubmit,
/// )
/// .unwrap();
/// cbb.copy_buffer(CopyBufferInfo::buffers(
///         temporary_accessible_buffer,
///         device_local_buffer.clone(),
///     ))
///     .unwrap();
/// let cb = cbb.build().unwrap();
///
/// // Execute copy command and wait for completion before proceeding.
/// cb.execute(queue.clone())
///     .unwrap()
///     .then_signal_fence_and_flush()
///     .unwrap()
///     .wait(None /* timeout */)
///     .unwrap()
/// ```
#[derive(Debug)]
pub struct DeviceLocalBuffer<T, A = PotentialDedicatedAllocation<StandardMemoryPoolAlloc>>
where
    T: BufferContents + ?Sized,
{
    // Inner content.
    inner: Arc<UnsafeBuffer>,

    // The memory held by the buffer.
    memory: A,

    // Queue families allowed to access this buffer.
    queue_family_indices: SmallVec<[u32; 4]>,

    // Necessary to make it compile.
    marker: PhantomData<Box<T>>,
}

impl<T> DeviceLocalBuffer<T>
where
    T: BufferContents,
{
    /// Builds a new buffer. Only allowed for sized data.
    ///
    /// # Panics
    ///
    /// - Panics if `T` has zero size.
    pub fn new(
        device: Arc<Device>,
        usage: BufferUsage,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<DeviceLocalBuffer<T>>, DeviceMemoryError> {
        unsafe {
            DeviceLocalBuffer::raw(
                device,
                size_of::<T>() as DeviceSize,
                usage,
                queue_family_indices,
            )
        }
    }
}

impl<T> DeviceLocalBuffer<T>
where
    T: BufferContents + ?Sized,
{
    /// Builds a `DeviceLocalBuffer` that copies its data from another buffer.
    ///
    /// This is a convenience function, equivalent to calling [`new`](DeviceLocalBuffer::new) with
    /// the queue family index of `command_buffer_builder`, then recording a `copy_buffer` command
    /// to `command_buffer_builder`.
    ///
    /// `command_buffer_builder` can then be used to record other commands, built, and executed as
    /// normal. If it is not executed, the buffer contents will be left undefined.
    ///
    /// # Panics
    ///
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub fn from_buffer<B, L, A>(
        source: Arc<B>,
        usage: BufferUsage,
        command_buffer_builder: &mut AutoCommandBufferBuilder<L, A>,
    ) -> Result<Arc<DeviceLocalBuffer<T>>, DeviceLocalBufferCreationError>
    where
        B: TypedBufferAccess<Content = T> + 'static,
        A: CommandBufferAllocator,
    {
        unsafe {
            // We automatically set `transfer_dst` to true in order to avoid annoying errors.
            let actual_usage = BufferUsage {
                transfer_dst: true,
                ..usage
            };

            let buffer = DeviceLocalBuffer::raw(
                source.device().clone(),
                source.size(),
                actual_usage,
                source
                    .device()
                    .active_queue_family_indices()
                    .iter()
                    .copied(),
            )?;

            command_buffer_builder
                .copy_buffer(CopyBufferInfo::buffers(source, buffer.clone()))
                .unwrap(); // TODO: return error?

            Ok(buffer)
        }
    }
}

impl<T> DeviceLocalBuffer<T>
where
    T: BufferContents,
{
    /// Builds a `DeviceLocalBuffer` from some data.
    ///
    /// This is a convenience function, equivalent to creating a `CpuAccessibleBuffer`, writing
    /// `data` to it, then calling [`from_buffer`](DeviceLocalBuffer::from_buffer) to copy the data
    /// over.
    ///
    /// # Panics
    ///
    /// - Panics if `T` has zero size.
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub fn from_data<L, A>(
        data: T,
        usage: BufferUsage,
        command_buffer_builder: &mut AutoCommandBufferBuilder<L, A>,
    ) -> Result<Arc<DeviceLocalBuffer<T>>, DeviceLocalBufferCreationError>
    where
        A: CommandBufferAllocator,
    {
        let source = CpuAccessibleBuffer::from_data(
            command_buffer_builder.device().clone(),
            BufferUsage {
                transfer_src: true,
                ..BufferUsage::empty()
            },
            false,
            data,
        )?;
        DeviceLocalBuffer::from_buffer(source, usage, command_buffer_builder)
    }
}

impl<T> DeviceLocalBuffer<[T]>
where
    [T]: BufferContents,
{
    /// Builds a `DeviceLocalBuffer` from an iterator of data.
    ///
    /// This is a convenience function, equivalent to creating a `CpuAccessibleBuffer`, writing
    /// `iter` to it, then calling [`from_buffer`](DeviceLocalBuffer::from_buffer) to copy the data
    /// over.
    ///
    /// # Panics
    ///
    /// - Panics if `T` has zero size.
    /// - Panics if `data` is empty.
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub fn from_iter<D, L, A>(
        data: D,
        usage: BufferUsage,
        command_buffer_builder: &mut AutoCommandBufferBuilder<L, A>,
    ) -> Result<Arc<DeviceLocalBuffer<[T]>>, DeviceLocalBufferCreationError>
    where
        D: IntoIterator<Item = T>,
        D::IntoIter: ExactSizeIterator,
        A: CommandBufferAllocator,
    {
        let source = CpuAccessibleBuffer::from_iter(
            command_buffer_builder.device().clone(),
            BufferUsage {
                transfer_src: true,
                ..BufferUsage::empty()
            },
            false,
            data,
        )?;
        DeviceLocalBuffer::from_buffer(source, usage, command_buffer_builder)
    }
}

impl<T> DeviceLocalBuffer<[T]>
where
    [T]: BufferContents,
{
    /// Builds a new buffer. Can be used for arrays.
    ///
    /// # Panics
    ///
    /// - Panics if `T` has zero size.
    /// - Panics if `len` is zero.
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub fn array(
        device: Arc<Device>,
        len: DeviceSize,
        usage: BufferUsage,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<DeviceLocalBuffer<[T]>>, DeviceMemoryError> {
        unsafe {
            DeviceLocalBuffer::raw(
                device,
                len * size_of::<T>() as DeviceSize,
                usage,
                queue_family_indices,
            )
        }
    }
}

impl<T> DeviceLocalBuffer<T>
where
    T: BufferContents + ?Sized,
{
    /// Builds a new buffer without checking the size.
    ///
    /// # Safety
    ///
    /// - You must ensure that the size that you pass is correct for `T`.
    ///
    /// # Panics
    ///
    /// - Panics if `size` is zero.
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub unsafe fn raw(
        device: Arc<Device>,
        size: DeviceSize,
        usage: BufferUsage,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<DeviceLocalBuffer<T>>, DeviceMemoryError> {
        let queue_family_indices: SmallVec<[_; 4]> = queue_family_indices.into_iter().collect();

        let (buffer, mem_reqs) = Self::build_buffer(&device, size, usage, &queue_family_indices)?;

        let memory = MemoryPool::alloc_from_requirements(
            &device.standard_memory_pool(),
            &mem_reqs,
            AllocLayout::Linear,
            MappingRequirement::DoNotMap,
            Some(DedicatedAllocation::Buffer(&buffer)),
            |t| {
                if t.property_flags.device_local {
                    AllocFromRequirementsFilter::Preferred
                } else {
                    AllocFromRequirementsFilter::Allowed
                }
            },
        )?;
        debug_assert!((memory.offset() % mem_reqs.alignment) == 0);
        buffer.bind_memory(memory.memory(), memory.offset())?;

        Ok(Arc::new(DeviceLocalBuffer {
            inner: buffer,
            memory,
            queue_family_indices,
            marker: PhantomData,
        }))
    }

    /// Same as `raw` but with exportable fd option for the allocated memory on Linux/BSD
    ///
    /// # Panics
    ///
    /// - Panics if `size` is zero.
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub unsafe fn raw_with_exportable_fd(
        device: Arc<Device>,
        size: DeviceSize,
        usage: BufferUsage,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<DeviceLocalBuffer<T>>, DeviceMemoryError> {
        assert!(device.enabled_extensions().khr_external_memory_fd);
        assert!(device.enabled_extensions().khr_external_memory);

        let queue_family_indices: SmallVec<[_; 4]> = queue_family_indices.into_iter().collect();

        let (buffer, mem_reqs) = Self::build_buffer(&device, size, usage, &queue_family_indices)?;

        let memory = alloc_dedicated_with_exportable_fd(
            device,
            &mem_reqs,
            AllocLayout::Linear,
            MappingRequirement::DoNotMap,
            DedicatedAllocation::Buffer(&buffer),
            |t| {
                if t.property_flags.device_local {
                    AllocFromRequirementsFilter::Preferred
                } else {
                    AllocFromRequirementsFilter::Allowed
                }
            },
        )?;
        let mem_offset = memory.offset();
        debug_assert!((mem_offset % mem_reqs.alignment) == 0);
        buffer.bind_memory(memory.memory(), mem_offset)?;

        Ok(Arc::new(DeviceLocalBuffer {
            inner: buffer,
            memory,
            queue_family_indices,
            marker: PhantomData,
        }))
    }

    unsafe fn build_buffer(
        device: &Arc<Device>,
        size: DeviceSize,
        usage: BufferUsage,
        queue_family_indices: &SmallVec<[u32; 4]>,
    ) -> Result<(Arc<UnsafeBuffer>, MemoryRequirements), DeviceMemoryError> {
        let buffer = {
            match UnsafeBuffer::new(
                device.clone(),
                UnsafeBufferCreateInfo {
                    sharing: if queue_family_indices.len() >= 2 {
                        Sharing::Concurrent(queue_family_indices.clone())
                    } else {
                        Sharing::Exclusive
                    },
                    size,
                    usage,
                    ..Default::default()
                },
            ) {
                Ok(b) => b,
                Err(BufferCreationError::AllocError(err)) => return Err(err),
                Err(_) => unreachable!(), // We don't use sparse binding, therefore the other
                                          // errors can't happen
            }
        };
        let mem_reqs = buffer.memory_requirements();

        Ok((buffer, mem_reqs))
    }

    /// Exports posix file descriptor for the allocated memory
    /// requires `khr_external_memory_fd` and `khr_external_memory` extensions to be loaded.
    /// Only works on Linux/BSD.
    pub fn export_posix_fd(&self) -> Result<File, DeviceMemoryError> {
        self.memory
            .memory()
            .export_fd(ExternalMemoryHandleType::OpaqueFd)
    }
}

impl<T, A> DeviceLocalBuffer<T, A>
where
    T: BufferContents + ?Sized,
{
    /// Returns the queue families this buffer can be used on.
    pub fn queue_family_indices(&self) -> &[u32] {
        &self.queue_family_indices
    }
}

unsafe impl<T, A> DeviceOwned for DeviceLocalBuffer<T, A>
where
    T: BufferContents + ?Sized,
{
    fn device(&self) -> &Arc<Device> {
        self.inner.device()
    }
}

unsafe impl<T, A> BufferAccess for DeviceLocalBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
    fn inner(&self) -> BufferInner<'_> {
        BufferInner {
            buffer: &self.inner,
            offset: 0,
        }
    }

    fn size(&self) -> DeviceSize {
        self.inner.size()
    }
}

impl<T, A> BufferAccessObject for Arc<DeviceLocalBuffer<T, A>>
where
    T: BufferContents + ?Sized,
    A: Send + Sync + 'static,
{
    fn as_buffer_access_object(&self) -> Arc<dyn BufferAccess> {
        self.clone()
    }
}

unsafe impl<T, A> TypedBufferAccess for DeviceLocalBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
    type Content = T;
}

impl<T, A> PartialEq for DeviceLocalBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner() == other.inner() && self.size() == other.size()
    }
}

impl<T, A> Eq for DeviceLocalBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
}

impl<T, A> Hash for DeviceLocalBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner().hash(state);
        self.size().hash(state);
    }
}

#[derive(Clone, Debug)]
pub enum DeviceLocalBufferCreationError {
    DeviceMemoryAllocationError(DeviceMemoryError),
    CommandBufferBeginError(CommandBufferBeginError),
}

impl Error for DeviceLocalBufferCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DeviceMemoryAllocationError(err) => Some(err),
            Self::CommandBufferBeginError(err) => Some(err),
        }
    }
}

impl Display for DeviceLocalBufferCreationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self {
            Self::DeviceMemoryAllocationError(err) => err.fmt(f),
            Self::CommandBufferBeginError(err) => err.fmt(f),
        }
    }
}

impl From<DeviceMemoryError> for DeviceLocalBufferCreationError {
    fn from(e: DeviceMemoryError) -> Self {
        Self::DeviceMemoryAllocationError(e)
    }
}

impl From<CommandBufferBeginError> for DeviceLocalBufferCreationError {
    fn from(e: CommandBufferBeginError) -> Self {
        Self::CommandBufferBeginError(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        command_buffer::{
            allocator::StandardCommandBufferAllocator, CommandBufferUsage, PrimaryCommandBuffer,
        },
        sync::GpuFuture,
    };

    #[test]
    fn from_data_working() {
        let (device, queue) = gfx_dev_and_queue!();

        let command_buffer_allocator = StandardCommandBufferAllocator::new(device.clone());
        let mut command_buffer_builder = AutoCommandBufferBuilder::primary(
            &command_buffer_allocator,
            queue.queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        )
        .unwrap();

        let buffer = DeviceLocalBuffer::from_data(
            12u32,
            BufferUsage {
                transfer_src: true,
                ..BufferUsage::empty()
            },
            &mut command_buffer_builder,
        )
        .unwrap();

        let destination = CpuAccessibleBuffer::from_data(
            device,
            BufferUsage {
                transfer_dst: true,
                ..BufferUsage::empty()
            },
            false,
            0,
        )
        .unwrap();

        command_buffer_builder
            .copy_buffer(CopyBufferInfo::buffers(buffer, destination.clone()))
            .unwrap();
        let _ = command_buffer_builder
            .build()
            .unwrap()
            .execute(queue)
            .unwrap()
            .then_signal_fence_and_flush()
            .unwrap();

        let destination_content = destination.read().unwrap();
        assert_eq!(*destination_content, 12);
    }

    #[test]
    fn from_iter_working() {
        let (device, queue) = gfx_dev_and_queue!();

        let command_buffer_allocator = StandardCommandBufferAllocator::new(device.clone());
        let mut command_buffer_builder = AutoCommandBufferBuilder::primary(
            &command_buffer_allocator,
            queue.queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        )
        .unwrap();

        let buffer = DeviceLocalBuffer::from_iter(
            (0..512u32).map(|n| n * 2),
            BufferUsage {
                transfer_src: true,
                ..BufferUsage::empty()
            },
            &mut command_buffer_builder,
        )
        .unwrap();

        let destination = CpuAccessibleBuffer::from_iter(
            device,
            BufferUsage {
                transfer_dst: true,
                ..BufferUsage::empty()
            },
            false,
            (0..512).map(|_| 0u32),
        )
        .unwrap();

        command_buffer_builder
            .copy_buffer(CopyBufferInfo::buffers(buffer, destination.clone()))
            .unwrap();
        let _ = command_buffer_builder
            .build()
            .unwrap()
            .execute(queue)
            .unwrap()
            .then_signal_fence_and_flush()
            .unwrap();

        let destination_content = destination.read().unwrap();
        for (n, &v) in destination_content.iter().enumerate() {
            assert_eq!(n * 2, v as usize);
        }
    }

    #[test]
    #[allow(unused)]
    fn create_buffer_zero_size_data() {
        let (device, queue) = gfx_dev_and_queue!();

        let command_buffer_allocator = StandardCommandBufferAllocator::new(device);
        let mut command_buffer_builder = AutoCommandBufferBuilder::primary(
            &command_buffer_allocator,
            queue.queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        )
        .unwrap();

        assert_should_panic!({
            DeviceLocalBuffer::from_data(
                (),
                BufferUsage {
                    transfer_dst: true,
                    ..BufferUsage::empty()
                },
                &mut command_buffer_builder,
            )
            .unwrap();
        });
    }

    // TODO: write tons of tests that try to exploit loopholes
    // this isn't possible yet because checks aren't correctly implemented yet
}
