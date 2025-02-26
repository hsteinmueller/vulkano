// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! Buffer whose content is accessible to the CPU.
//!
//! The `CpuAccessibleBuffer` is a basic general-purpose buffer. It can be used in any situation
//! but may not perform as well as other buffer types.
//!
//! Each access from the CPU or from the GPU locks the whole buffer for either reading or writing.
//! You can read the buffer multiple times simultaneously. Trying to read and write simultaneously,
//! or write and write simultaneously will block.

use super::{
    sys::UnsafeBuffer, BufferAccess, BufferAccessObject, BufferContents, BufferInner, BufferUsage,
};
use crate::{
    buffer::{sys::UnsafeBufferCreateInfo, BufferCreationError, TypedBufferAccess},
    device::{Device, DeviceOwned},
    memory::{
        pool::{
            AllocFromRequirementsFilter, AllocLayout, MappingRequirement, MemoryPoolAlloc,
            PotentialDedicatedAllocation, StandardMemoryPoolAlloc,
        },
        DedicatedAllocation, DeviceMemoryError, MemoryPool,
    },
    sync::Sharing,
    DeviceSize,
};
use smallvec::SmallVec;
use std::{
    error::Error,
    fmt::{Display, Error as FmtError, Formatter},
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::size_of,
    ops::{Deref, DerefMut, Range},
    ptr,
    sync::Arc,
};

/// Buffer whose content is accessible by the CPU.
///
/// Setting the `host_cached` field on the various initializers to `true` will make it so
/// the `CpuAccessibleBuffer` prefers to allocate from host_cached memory. Host cached
/// memory caches GPU data on the CPU side. This can be more performant in cases where
/// the cpu needs to read data coming off the GPU.
#[derive(Debug)]
pub struct CpuAccessibleBuffer<T, A = PotentialDedicatedAllocation<StandardMemoryPoolAlloc>>
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

impl<T> CpuAccessibleBuffer<T>
where
    T: BufferContents,
{
    /// Builds a new buffer with some data in it. Only allowed for sized data.
    ///
    /// # Panics
    ///
    /// - Panics if `T` has zero size.
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub fn from_data(
        device: Arc<Device>,
        usage: BufferUsage,
        host_cached: bool,
        data: T,
    ) -> Result<Arc<CpuAccessibleBuffer<T>>, DeviceMemoryError> {
        unsafe {
            let uninitialized = CpuAccessibleBuffer::raw(
                device,
                size_of::<T>() as DeviceSize,
                usage,
                host_cached,
                [],
            )?;

            // Note that we are in panic-unsafety land here. However a panic should never ever
            // happen here, so in theory we are safe.
            // TODO: check whether that's true ^

            {
                let mut mapping = uninitialized.write().unwrap();
                ptr::write::<T>(&mut *mapping, data)
            }

            Ok(uninitialized)
        }
    }

    /// Builds a new uninitialized buffer. Only allowed for sized data.
    ///
    /// # Panics
    ///
    /// - Panics if `T` has zero size.
    pub unsafe fn uninitialized(
        device: Arc<Device>,
        usage: BufferUsage,
        host_cached: bool,
    ) -> Result<Arc<CpuAccessibleBuffer<T>>, DeviceMemoryError> {
        CpuAccessibleBuffer::raw(device, size_of::<T>() as DeviceSize, usage, host_cached, [])
    }
}

impl<T> CpuAccessibleBuffer<[T]>
where
    [T]: BufferContents,
{
    /// Builds a new buffer that contains an array `T`. The initial data comes from an iterator
    /// that produces that list of Ts.
    ///
    /// # Panics
    ///
    /// - Panics if `T` has zero size.
    /// - Panics if `data` is empty.
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub fn from_iter<I>(
        device: Arc<Device>,
        usage: BufferUsage,
        host_cached: bool,
        data: I,
    ) -> Result<Arc<CpuAccessibleBuffer<[T]>>, DeviceMemoryError>
    where
        I: IntoIterator<Item = T>,
        I::IntoIter: ExactSizeIterator,
    {
        let data = data.into_iter();

        unsafe {
            let uninitialized = CpuAccessibleBuffer::uninitialized_array(
                device,
                data.len() as DeviceSize,
                usage,
                host_cached,
            )?;

            // Note that we are in panic-unsafety land here. However a panic should never ever
            // happen here, so in theory we are safe.
            // TODO: check whether that's true ^

            {
                let mut mapping = uninitialized.write().unwrap();

                for (i, o) in data.zip(mapping.iter_mut()) {
                    ptr::write(o, i);
                }
            }

            Ok(uninitialized)
        }
    }

    /// Builds a new buffer. Can be used for arrays.
    ///
    /// # Panics
    ///
    /// - Panics if `T` has zero size.
    /// - Panics if `len` is zero.
    /// - Panics if `usage.shader_device_address` is `true`.
    // TODO: ^
    pub unsafe fn uninitialized_array(
        device: Arc<Device>,
        len: DeviceSize,
        usage: BufferUsage,
        host_cached: bool,
    ) -> Result<Arc<CpuAccessibleBuffer<[T]>>, DeviceMemoryError> {
        CpuAccessibleBuffer::raw(
            device,
            len * size_of::<T>() as DeviceSize,
            usage,
            host_cached,
            [],
        )
    }
}

impl<T> CpuAccessibleBuffer<T>
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
        host_cached: bool,
        queue_family_indices: impl IntoIterator<Item = u32>,
    ) -> Result<Arc<CpuAccessibleBuffer<T>>, DeviceMemoryError> {
        let queue_family_indices: SmallVec<[_; 4]> = queue_family_indices.into_iter().collect();

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

        let memory = MemoryPool::alloc_from_requirements(
            &device.standard_memory_pool(),
            &mem_reqs,
            AllocLayout::Linear,
            MappingRequirement::Map,
            Some(DedicatedAllocation::Buffer(&buffer)),
            |m| {
                if m.property_flags.host_cached {
                    if host_cached {
                        AllocFromRequirementsFilter::Preferred
                    } else {
                        AllocFromRequirementsFilter::Allowed
                    }
                } else {
                    if host_cached {
                        AllocFromRequirementsFilter::Allowed
                    } else {
                        AllocFromRequirementsFilter::Preferred
                    }
                }
            },
        )?;
        debug_assert!((memory.offset() % mem_reqs.alignment) == 0);
        debug_assert!(memory.mapped_memory().is_some());
        buffer.bind_memory(memory.memory(), memory.offset())?;

        Ok(Arc::new(CpuAccessibleBuffer {
            inner: buffer,
            memory,
            queue_family_indices,
            marker: PhantomData,
        }))
    }
}

impl<T, A> CpuAccessibleBuffer<T, A>
where
    T: BufferContents + ?Sized,
{
    /// Returns the queue families this buffer can be used on.
    pub fn queue_family_indices(&self) -> &[u32] {
        &self.queue_family_indices
    }
}

impl<T, A> CpuAccessibleBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: MemoryPoolAlloc,
{
    /// Locks the buffer in order to read its content from the CPU.
    ///
    /// If the buffer is currently used in exclusive mode by the GPU, this function will return
    /// an error. Similarly if you called `write()` on the buffer and haven't dropped the lock,
    /// this function will return an error as well.
    ///
    /// After this function successfully locks the buffer, any attempt to submit a command buffer
    /// that uses it in exclusive mode will fail. You can still submit this buffer for non-exclusive
    /// accesses (ie. reads).
    pub fn read(&self) -> Result<ReadLock<'_, T, A>, ReadLockError> {
        let mut state = self.inner.state();
        let buffer_range = self.inner().offset..self.inner().offset + self.size();

        unsafe {
            state.check_cpu_read(buffer_range.clone())?;
            state.cpu_read_lock(buffer_range.clone());
        }

        let mapped_memory = self.memory.mapped_memory().unwrap();
        let offset = self.memory.offset();
        let memory_range = offset..offset + self.inner.size();

        let bytes = unsafe {
            // If there are other read locks being held at this point, they also called
            // `invalidate_range` when locking. The GPU can't write data while the CPU holds a read
            // lock, so there will no new data and this call will do nothing.
            // TODO: probably still more efficient to call it only if we're the first to acquire a
            // read lock, but the number of CPU locks isn't currently tracked anywhere.
            mapped_memory
                .invalidate_range(memory_range.clone())
                .unwrap();
            mapped_memory.read(memory_range).unwrap()
        };

        Ok(ReadLock {
            inner: self,
            buffer_range,
            data: T::from_bytes(bytes).unwrap(),
        })
    }

    /// Locks the buffer in order to write its content from the CPU.
    ///
    /// If the buffer is currently in use by the GPU, this function will return an error. Similarly
    /// if you called `read()` on the buffer and haven't dropped the lock, this function will
    /// return an error as well.
    ///
    /// After this function successfully locks the buffer, any attempt to submit a command buffer
    /// that uses it and any attempt to call `read()` will return an error.
    pub fn write(&self) -> Result<WriteLock<'_, T, A>, WriteLockError> {
        let mut state = self.inner.state();
        let buffer_range = self.inner().offset..self.inner().offset + self.size();

        unsafe {
            state.check_cpu_write(buffer_range.clone())?;
            state.cpu_write_lock(buffer_range.clone());
        }

        let mapped_memory = self.memory.mapped_memory().unwrap();
        let offset = self.memory.offset();
        let memory_range = offset..offset + self.size();

        let bytes = unsafe {
            mapped_memory
                .invalidate_range(memory_range.clone())
                .unwrap();
            mapped_memory.write(memory_range.clone()).unwrap()
        };

        Ok(WriteLock {
            inner: self,
            buffer_range,
            memory_range,
            data: T::from_bytes_mut(bytes).unwrap(),
        })
    }
}

unsafe impl<T, A> BufferAccess for CpuAccessibleBuffer<T, A>
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

impl<T, A> BufferAccessObject for Arc<CpuAccessibleBuffer<T, A>>
where
    T: BufferContents + ?Sized,
    A: Send + Sync + 'static,
{
    fn as_buffer_access_object(&self) -> Arc<dyn BufferAccess> {
        self.clone()
    }
}

unsafe impl<T, A> TypedBufferAccess for CpuAccessibleBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
    type Content = T;
}

unsafe impl<T, A> DeviceOwned for CpuAccessibleBuffer<T, A>
where
    T: BufferContents + ?Sized,
{
    fn device(&self) -> &Arc<Device> {
        self.inner.device()
    }
}

impl<T, A> PartialEq for CpuAccessibleBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner() == other.inner() && self.size() == other.size()
    }
}

impl<T, A> Eq for CpuAccessibleBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
}

impl<T, A> Hash for CpuAccessibleBuffer<T, A>
where
    T: BufferContents + ?Sized,
    A: Send + Sync,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner().hash(state);
        self.size().hash(state);
    }
}

/// Object that can be used to read or write the content of a `CpuAccessibleBuffer`.
///
/// Note that this object holds a rwlock read guard on the chunk. If another thread tries to access
/// this buffer's content or tries to submit a GPU command that uses this buffer, it will block.
#[derive(Debug)]
pub struct ReadLock<'a, T, A>
where
    T: BufferContents + ?Sized,
    A: MemoryPoolAlloc,
{
    inner: &'a CpuAccessibleBuffer<T, A>,
    buffer_range: Range<DeviceSize>,
    data: &'a T,
}

impl<'a, T, A> Drop for ReadLock<'a, T, A>
where
    T: BufferContents + ?Sized + 'a,
    A: MemoryPoolAlloc,
{
    fn drop(&mut self) {
        unsafe {
            let mut state = self.inner.inner.state();
            state.cpu_read_unlock(self.buffer_range.clone());
        }
    }
}

impl<'a, T, A> Deref for ReadLock<'a, T, A>
where
    T: BufferContents + ?Sized + 'a,
    A: MemoryPoolAlloc,
{
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

/// Object that can be used to read or write the content of a `CpuAccessibleBuffer`.
///
/// Note that this object holds a rwlock write guard on the chunk. If another thread tries to access
/// this buffer's content or tries to submit a GPU command that uses this buffer, it will block.
#[derive(Debug)]
pub struct WriteLock<'a, T, A>
where
    T: BufferContents + ?Sized,
    A: MemoryPoolAlloc,
{
    inner: &'a CpuAccessibleBuffer<T, A>,
    buffer_range: Range<DeviceSize>,
    memory_range: Range<DeviceSize>,
    data: &'a mut T,
}

impl<'a, T, A> Drop for WriteLock<'a, T, A>
where
    T: BufferContents + ?Sized + 'a,
    A: MemoryPoolAlloc,
{
    fn drop(&mut self) {
        unsafe {
            self.inner
                .memory
                .mapped_memory()
                .unwrap()
                .flush_range(self.memory_range.clone())
                .unwrap();

            let mut state = self.inner.inner.state();
            state.cpu_write_unlock(self.buffer_range.clone());
        }
    }
}

impl<'a, T, A> Deref for WriteLock<'a, T, A>
where
    T: BufferContents + ?Sized + 'a,
    A: MemoryPoolAlloc,
{
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

impl<'a, T, A> DerefMut for WriteLock<'a, T, A>
where
    T: BufferContents + ?Sized + 'a,
    A: MemoryPoolAlloc,
{
    fn deref_mut(&mut self) -> &mut T {
        self.data
    }
}

/// Error when attempting to CPU-read a buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadLockError {
    /// The buffer is already locked for write mode by the CPU.
    CpuWriteLocked,
    /// The buffer is already locked for write mode by the GPU.
    GpuWriteLocked,
}

impl Error for ReadLockError {}

impl Display for ReadLockError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        write!(
            f,
            "{}",
            match self {
                ReadLockError::CpuWriteLocked => {
                    "the buffer is already locked for write mode by the CPU"
                }
                ReadLockError::GpuWriteLocked => {
                    "the buffer is already locked for write mode by the GPU"
                }
            }
        )
    }
}

/// Error when attempting to CPU-write a buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteLockError {
    /// The buffer is already locked by the CPU.
    CpuLocked,
    /// The buffer is already locked by the GPU.
    GpuLocked,
}

impl Error for WriteLockError {}

impl Display for WriteLockError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        write!(
            f,
            "{}",
            match self {
                WriteLockError::CpuLocked => "the buffer is already locked by the CPU",
                WriteLockError::GpuLocked => "the buffer is already locked by the GPU",
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::buffer::{BufferUsage, CpuAccessibleBuffer};

    #[test]
    fn create_empty_buffer() {
        let (device, _queue) = gfx_dev_and_queue!();

        const EMPTY: [i32; 0] = [];

        assert_should_panic!({
            CpuAccessibleBuffer::from_data(
                device.clone(),
                BufferUsage {
                    transfer_dst: true,
                    ..BufferUsage::empty()
                },
                false,
                EMPTY,
            )
            .unwrap();
            CpuAccessibleBuffer::from_iter(
                device,
                BufferUsage {
                    transfer_dst: true,
                    ..BufferUsage::empty()
                },
                false,
                EMPTY.into_iter(),
            )
            .unwrap();
        });
    }
}
