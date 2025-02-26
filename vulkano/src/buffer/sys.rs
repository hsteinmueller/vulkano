// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! Low level implementation of buffers.
//!
//! Wraps directly around Vulkan buffers, with the exceptions of a few safety checks.
//!
//! The `UnsafeBuffer` type is the lowest-level buffer object provided by this library. It is used
//! internally by the higher-level buffer types. You are strongly encouraged to have excellent
//! knowledge of the Vulkan specs if you want to use an `UnsafeBuffer`.
//!
//! Here is what you must take care of when you use an `UnsafeBuffer`:
//!
//! - Synchronization, ie. avoid reading and writing simultaneously to the same buffer.
//! - Memory aliasing considerations. If you use the same memory to back multiple resources, you
//!   must ensure that they are not used together and must enable some additional flags.
//! - Binding memory correctly and only once. If you use sparse binding, respect the rules of
//!   sparse binding.
//! - Type safety.

use super::{
    cpu_access::{ReadLockError, WriteLockError},
    BufferUsage,
};
use crate::{
    device::{Device, DeviceOwned},
    macros::vulkan_bitflags,
    memory::{DeviceMemory, DeviceMemoryError, ExternalMemoryHandleTypes, MemoryRequirements},
    range_map::RangeMap,
    sync::{AccessError, CurrentAccess, Sharing},
    DeviceSize, OomError, RequirementNotMet, RequiresOneOf, Version, VulkanError, VulkanObject,
};
use ash::vk::Handle;
use parking_lot::{Mutex, MutexGuard};
use smallvec::SmallVec;
use std::{
    error::Error,
    fmt::{Display, Error as FmtError, Formatter},
    hash::{Hash, Hasher},
    mem::MaybeUninit,
    ops::Range,
    ptr,
    sync::Arc,
};

/// Data storage in a GPU-accessible location.
#[derive(Debug)]
pub struct UnsafeBuffer {
    handle: ash::vk::Buffer,
    device: Arc<Device>,

    size: DeviceSize,
    usage: BufferUsage,
    external_memory_handle_types: ExternalMemoryHandleTypes,

    state: Mutex<BufferState>,
}

impl UnsafeBuffer {
    /// Creates a new `UnsafeBuffer`.
    ///
    /// # Panics
    ///
    /// - Panics if `create_info.sharing` is [`Concurrent`](Sharing::Concurrent) with less than 2
    ///   items.
    /// - Panics if `create_info.size` is zero.
    /// - Panics if `create_info.usage` is empty.
    #[inline]
    pub fn new(
        device: Arc<Device>,
        mut create_info: UnsafeBufferCreateInfo,
    ) -> Result<Arc<Self>, BufferCreationError> {
        match &mut create_info.sharing {
            Sharing::Exclusive => (),
            Sharing::Concurrent(queue_family_indices) => {
                // VUID-VkBufferCreateInfo-sharingMode-01419
                queue_family_indices.sort_unstable();
                queue_family_indices.dedup();
            }
        }

        Self::validate_new(&device, &create_info)?;

        unsafe { Ok(Self::new_unchecked(device, create_info)?) }
    }

    fn validate_new(
        device: &Device,
        create_info: &UnsafeBufferCreateInfo,
    ) -> Result<(), BufferCreationError> {
        let &UnsafeBufferCreateInfo {
            ref sharing,
            size,
            sparse,
            usage,
            external_memory_handle_types,
            _ne: _,
        } = create_info;

        // VUID-VkBufferCreateInfo-usage-parameter
        usage.validate_device(device)?;

        // VUID-VkBufferCreateInfo-usage-requiredbitmask
        assert!(!usage.is_empty());

        // VUID-VkBufferCreateInfo-size-00912
        assert!(size != 0);

        if let Some(sparse_level) = sparse {
            // VUID-VkBufferCreateInfo-flags-00915
            if !device.enabled_features().sparse_binding {
                return Err(BufferCreationError::RequirementNotMet {
                    required_for: "`create_info.sparse` is `Some`",
                    requires_one_of: RequiresOneOf {
                        features: &["sparse_binding"],
                        ..Default::default()
                    },
                });
            }

            // VUID-VkBufferCreateInfo-flags-00916
            if sparse_level.sparse_residency && !device.enabled_features().sparse_residency_buffer {
                return Err(BufferCreationError::RequirementNotMet {
                    required_for: "`create_info.sparse` is `Some(sparse_level)`, where `sparse_level.sparse_residency` is set",
                    requires_one_of: RequiresOneOf {
                        features: &["sparse_residency_buffer"],
                        ..Default::default()
                    },
                });
            }

            // VUID-VkBufferCreateInfo-flags-00917
            if sparse_level.sparse_aliased && !device.enabled_features().sparse_residency_aliased {
                return Err(BufferCreationError::RequirementNotMet {
                    required_for: "`create_info.sparse` is `Some(sparse_level)`, where `sparse_level.sparse_aliased` is set",
                    requires_one_of: RequiresOneOf {
                        features: &["sparse_residency_aliased"],
                        ..Default::default()
                    },
                });
            }

            // VUID-VkBufferCreateInfo-flags-00918
        }

        match sharing {
            Sharing::Exclusive => (),
            Sharing::Concurrent(queue_family_indices) => {
                // VUID-VkBufferCreateInfo-sharingMode-00914
                assert!(queue_family_indices.len() >= 2);

                for &queue_family_index in queue_family_indices.iter() {
                    // VUID-VkBufferCreateInfo-sharingMode-01419
                    if queue_family_index
                        >= device.physical_device().queue_family_properties().len() as u32
                    {
                        return Err(BufferCreationError::SharingQueueFamilyIndexOutOfRange {
                            queue_family_index,
                            queue_family_count: device
                                .physical_device()
                                .queue_family_properties()
                                .len() as u32,
                        });
                    }
                }
            }
        }

        if let Some(max_buffer_size) = device.physical_device().properties().max_buffer_size {
            // VUID-VkBufferCreateInfo-size-06409
            if size > max_buffer_size {
                return Err(BufferCreationError::MaxBufferSizeExceeded {
                    size,
                    max: max_buffer_size,
                });
            }
        }

        if !external_memory_handle_types.is_empty() {
            if !(device.api_version() >= Version::V1_1
                || device.enabled_extensions().khr_external_memory)
            {
                return Err(BufferCreationError::RequirementNotMet {
                    required_for: "`create_info.external_memory_handle_types` is not empty",
                    requires_one_of: RequiresOneOf {
                        api_version: Some(Version::V1_1),
                        device_extensions: &["khr_external_memory"],
                        ..Default::default()
                    },
                });
            }

            // VUID-VkExternalMemoryBufferCreateInfo-handleTypes-parameter
            external_memory_handle_types.validate_device(device)?;

            // VUID-VkBufferCreateInfo-pNext-00920
            // TODO:
        }

        Ok(())
    }

    #[cfg_attr(not(feature = "document_unchecked"), doc(hidden))]
    pub unsafe fn new_unchecked(
        device: Arc<Device>,
        create_info: UnsafeBufferCreateInfo,
    ) -> Result<Arc<Self>, VulkanError> {
        let &UnsafeBufferCreateInfo {
            ref sharing,
            size,
            sparse,
            usage,
            external_memory_handle_types,
            _ne: _,
        } = &create_info;

        let mut flags = ash::vk::BufferCreateFlags::empty();

        if let Some(sparse_level) = sparse {
            flags |= sparse_level.into();
        }

        let (sharing_mode, queue_family_index_count, p_queue_family_indices) = match sharing {
            Sharing::Exclusive => (ash::vk::SharingMode::EXCLUSIVE, 0, &[] as _),
            Sharing::Concurrent(queue_family_indices) => (
                ash::vk::SharingMode::CONCURRENT,
                queue_family_indices.len() as u32,
                queue_family_indices.as_ptr(),
            ),
        };

        let mut create_info_vk = ash::vk::BufferCreateInfo {
            flags,
            size,
            usage: usage.into(),
            sharing_mode,
            queue_family_index_count,
            p_queue_family_indices,
            ..Default::default()
        };
        let mut external_memory_info_vk = None;

        if !external_memory_handle_types.is_empty() {
            let _ = external_memory_info_vk.insert(ash::vk::ExternalMemoryBufferCreateInfo {
                handle_types: external_memory_handle_types.into(),
                ..Default::default()
            });
        }

        if let Some(next) = external_memory_info_vk.as_mut() {
            next.p_next = create_info_vk.p_next;
            create_info_vk.p_next = next as *const _ as *const _;
        }

        let handle = {
            let fns = device.fns();
            let mut output = MaybeUninit::uninit();
            (fns.v1_0.create_buffer)(
                device.internal_object(),
                &create_info_vk,
                ptr::null(),
                output.as_mut_ptr(),
            )
            .result()
            .map_err(VulkanError::from)?;
            output.assume_init()
        };

        Ok(Self::from_handle(device, handle, create_info))
    }

    /// Creates a new `UnsafeBuffer` from a raw object handle.
    ///
    /// # Safety
    ///
    /// - `handle` must be a valid Vulkan object handle created from `device`.
    /// - `create_info` must match the info used to create the object.
    #[inline]
    pub unsafe fn from_handle(
        device: Arc<Device>,
        handle: ash::vk::Buffer,
        create_info: UnsafeBufferCreateInfo,
    ) -> Arc<Self> {
        let UnsafeBufferCreateInfo {
            size,
            usage,
            sharing: _,
            sparse: _,
            external_memory_handle_types,
            _ne: _,
        } = create_info;

        Arc::new(UnsafeBuffer {
            handle,
            device,

            size,
            usage,
            external_memory_handle_types,

            state: Mutex::new(BufferState::new(size)),
        })
    }

    /// Returns the memory requirements for this buffer.
    pub fn memory_requirements(&self) -> MemoryRequirements {
        fn align(val: DeviceSize, al: DeviceSize) -> DeviceSize {
            al * (1 + (val - 1) / al)
        }

        let buffer_memory_requirements_info2 = ash::vk::BufferMemoryRequirementsInfo2 {
            buffer: self.handle,
            ..Default::default()
        };
        let mut memory_requirements2 = ash::vk::MemoryRequirements2::default();

        let mut memory_dedicated_requirements = if self.device.api_version() >= Version::V1_1
            || self.device.enabled_extensions().khr_dedicated_allocation
        {
            Some(ash::vk::MemoryDedicatedRequirementsKHR::default())
        } else {
            None
        };

        if let Some(next) = memory_dedicated_requirements.as_mut() {
            next.p_next = memory_requirements2.p_next;
            memory_requirements2.p_next = next as *mut _ as *mut _;
        }

        unsafe {
            let fns = self.device.fns();

            if self.device.api_version() >= Version::V1_1
                || self
                    .device
                    .enabled_extensions()
                    .khr_get_memory_requirements2
            {
                if self.device.api_version() >= Version::V1_1 {
                    (fns.v1_1.get_buffer_memory_requirements2)(
                        self.device.internal_object(),
                        &buffer_memory_requirements_info2,
                        &mut memory_requirements2,
                    );
                } else {
                    (fns.khr_get_memory_requirements2
                        .get_buffer_memory_requirements2_khr)(
                        self.device.internal_object(),
                        &buffer_memory_requirements_info2,
                        &mut memory_requirements2,
                    );
                }
            } else {
                (fns.v1_0.get_buffer_memory_requirements)(
                    self.device.internal_object(),
                    self.handle,
                    &mut memory_requirements2.memory_requirements,
                );
            }
        }

        debug_assert!(memory_requirements2.memory_requirements.size >= self.size);
        debug_assert!(memory_requirements2.memory_requirements.memory_type_bits != 0);

        let mut memory_requirements = MemoryRequirements {
            prefer_dedicated: memory_dedicated_requirements
                .map_or(false, |dreqs| dreqs.prefers_dedicated_allocation != 0),
            ..MemoryRequirements::from(memory_requirements2.memory_requirements)
        };

        // We have to manually enforce some additional requirements for some buffer types.
        let properties = self.device.physical_device().properties();
        if self.usage.uniform_texel_buffer || self.usage.storage_texel_buffer {
            memory_requirements.alignment = align(
                memory_requirements.alignment,
                properties.min_texel_buffer_offset_alignment,
            );
        }

        if self.usage.storage_buffer {
            memory_requirements.alignment = align(
                memory_requirements.alignment,
                properties.min_storage_buffer_offset_alignment,
            );
        }

        if self.usage.uniform_buffer {
            memory_requirements.alignment = align(
                memory_requirements.alignment,
                properties.min_uniform_buffer_offset_alignment,
            );
        }

        memory_requirements
    }

    /// Binds device memory to this buffer.
    ///
    /// # Panics
    ///
    /// - Panics if `self.usage.shader_device_address` is `true` and the `memory` was not allocated
    ///   with the [`device_address`] flag set and the [`ext_buffer_device_address`] extension is
    ///   not enabled on the device.
    ///
    /// [`device_address`]: crate::memory::MemoryAllocateFlags::device_address
    /// [`ext_buffer_device_address`]: crate::device::DeviceExtensions::ext_buffer_device_address
    pub unsafe fn bind_memory(
        &self,
        memory: &DeviceMemory,
        offset: DeviceSize,
    ) -> Result<(), OomError> {
        let fns = self.device.fns();

        // We check for correctness in debug mode.
        debug_assert!({
            let mut mem_reqs = MaybeUninit::uninit();
            (fns.v1_0.get_buffer_memory_requirements)(
                self.device.internal_object(),
                self.handle,
                mem_reqs.as_mut_ptr(),
            );

            let mem_reqs = mem_reqs.assume_init();
            mem_reqs.size <= (memory.allocation_size() - offset)
                && (offset % mem_reqs.alignment) == 0
                && mem_reqs.memory_type_bits & (1 << memory.memory_type_index()) != 0
        });

        // Check for alignment correctness.
        {
            let properties = self.device().physical_device().properties();
            if self.usage().uniform_texel_buffer || self.usage().storage_texel_buffer {
                debug_assert!(offset % properties.min_texel_buffer_offset_alignment == 0);
            }
            if self.usage().storage_buffer {
                debug_assert!(offset % properties.min_storage_buffer_offset_alignment == 0);
            }
            if self.usage().uniform_buffer {
                debug_assert!(offset % properties.min_uniform_buffer_offset_alignment == 0);
            }
        }

        // VUID-vkBindBufferMemory-bufferDeviceAddress-03339
        if self.usage.shader_device_address
            && !self.device.enabled_extensions().ext_buffer_device_address
        {
            assert!(memory.flags().device_address);
        }

        (fns.v1_0.bind_buffer_memory)(
            self.device.internal_object(),
            self.handle,
            memory.internal_object(),
            offset,
        )
        .result()
        .map_err(VulkanError::from)?;

        Ok(())
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, BufferState> {
        self.state.lock()
    }

    /// Returns the size of the buffer in bytes.
    #[inline]
    pub fn size(&self) -> DeviceSize {
        self.size
    }

    /// Returns the usage the buffer was created with.
    #[inline]
    pub fn usage(&self) -> &BufferUsage {
        &self.usage
    }

    /// Returns the external memory handle types that are supported with this buffer.
    #[inline]
    pub fn external_memory_handle_types(&self) -> ExternalMemoryHandleTypes {
        self.external_memory_handle_types
    }

    /// Returns a key unique to each `UnsafeBuffer`. Can be used for the `conflicts_key` method.
    #[inline]
    pub fn key(&self) -> u64 {
        self.handle.as_raw()
    }
}

impl Drop for UnsafeBuffer {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let fns = self.device.fns();
            (fns.v1_0.destroy_buffer)(self.device.internal_object(), self.handle, ptr::null());
        }
    }
}

unsafe impl VulkanObject for UnsafeBuffer {
    type Object = ash::vk::Buffer;

    #[inline]
    fn internal_object(&self) -> ash::vk::Buffer {
        self.handle
    }
}

unsafe impl DeviceOwned for UnsafeBuffer {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

impl PartialEq for UnsafeBuffer {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.handle == other.handle && self.device == other.device
    }
}

impl Eq for UnsafeBuffer {}

impl Hash for UnsafeBuffer {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.handle.hash(state);
        self.device.hash(state);
    }
}

/// Parameters to create a new `UnsafeBuffer`.
#[derive(Clone, Debug)]
pub struct UnsafeBufferCreateInfo {
    /// Whether the buffer can be shared across multiple queues, or is limited to a single queue.
    ///
    /// The default value is [`Sharing::Exclusive`].
    pub sharing: Sharing<SmallVec<[u32; 4]>>,

    /// The size in bytes of the buffer.
    ///
    /// The default value is `0`, which must be overridden.
    pub size: DeviceSize,

    /// Create a buffer with sparsely bound memory.
    ///
    /// The default value is `None`.
    pub sparse: Option<SparseLevel>,

    /// How the buffer is going to be used.
    ///
    /// The default value is [`BufferUsage::empty()`], which must be overridden.
    pub usage: BufferUsage,

    /// The external memory handle types that are going to be used with the buffer.
    ///
    /// If any of the fields in this value are set, the device must either support API version 1.1
    /// or the [`khr_external_memory`](crate::device::DeviceExtensions::khr_external_memory)
    /// extension must be enabled.
    ///
    /// The default value is [`ExternalMemoryHandleTypes::empty()`].
    pub external_memory_handle_types: ExternalMemoryHandleTypes,

    pub _ne: crate::NonExhaustive,
}

impl Default for UnsafeBufferCreateInfo {
    #[inline]
    fn default() -> Self {
        Self {
            sharing: Sharing::Exclusive,
            size: 0,
            sparse: None,
            usage: BufferUsage::empty(),
            external_memory_handle_types: ExternalMemoryHandleTypes::empty(),
            _ne: crate::NonExhaustive(()),
        }
    }
}

/// Error that can happen when creating a buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferCreationError {
    /// Allocating memory failed.
    AllocError(DeviceMemoryError),

    RequirementNotMet {
        required_for: &'static str,
        requires_one_of: RequiresOneOf,
    },

    /// The specified size exceeded the value of the `max_buffer_size` limit.
    MaxBufferSizeExceeded { size: DeviceSize, max: DeviceSize },

    /// The sharing mode was set to `Concurrent`, but one of the specified queue family indices was
    /// out of range.
    SharingQueueFamilyIndexOutOfRange {
        queue_family_index: u32,
        queue_family_count: u32,
    },
}

impl Error for BufferCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            BufferCreationError::AllocError(err) => Some(err),
            _ => None,
        }
    }
}

impl Display for BufferCreationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self {
            Self::AllocError(_) => write!(f, "allocating memory failed"),
            Self::RequirementNotMet {
                required_for,
                requires_one_of,
            } => write!(
                f,
                "a requirement was not met for: {}; requires one of: {}",
                required_for, requires_one_of,
            ),
            Self::MaxBufferSizeExceeded { .. } => write!(
                f,
                "the specified size exceeded the value of the `max_buffer_size` limit",
            ),
            Self::SharingQueueFamilyIndexOutOfRange { .. } => write!(
                f,
                "the sharing mode was set to `Concurrent`, but one of the specified queue family \
                indices was out of range",
            ),
        }
    }
}

impl From<OomError> for BufferCreationError {
    fn from(err: OomError) -> BufferCreationError {
        BufferCreationError::AllocError(err.into())
    }
}

impl From<VulkanError> for BufferCreationError {
    fn from(err: VulkanError) -> BufferCreationError {
        match err {
            err @ VulkanError::OutOfHostMemory => {
                BufferCreationError::AllocError(DeviceMemoryError::from(err))
            }
            err @ VulkanError::OutOfDeviceMemory => {
                BufferCreationError::AllocError(DeviceMemoryError::from(err))
            }
            _ => panic!("unexpected error: {:?}", err),
        }
    }
}

impl From<RequirementNotMet> for BufferCreationError {
    fn from(err: RequirementNotMet) -> Self {
        Self::RequirementNotMet {
            required_for: err.required_for,
            requires_one_of: err.requires_one_of,
        }
    }
}

vulkan_bitflags! {
    /// The level of sparse binding that a buffer should be created with.
    #[non_exhaustive]
    SparseLevel = BufferCreateFlags(u32);

    // TODO: document
    sparse_residency = SPARSE_ALIASED,

    // TODO: document
    sparse_aliased = SPARSE_ALIASED,
}

/// The current state of a buffer.
#[derive(Debug)]
pub(crate) struct BufferState {
    ranges: RangeMap<DeviceSize, BufferRangeState>,
}

impl BufferState {
    fn new(size: DeviceSize) -> Self {
        BufferState {
            ranges: [(
                0..size,
                BufferRangeState {
                    current_access: CurrentAccess::Shared {
                        cpu_reads: 0,
                        gpu_reads: 0,
                    },
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    pub(crate) fn check_cpu_read(&self, range: Range<DeviceSize>) -> Result<(), ReadLockError> {
        for (_range, state) in self.ranges.range(&range) {
            match &state.current_access {
                CurrentAccess::CpuExclusive { .. } => return Err(ReadLockError::CpuWriteLocked),
                CurrentAccess::GpuExclusive { .. } => return Err(ReadLockError::GpuWriteLocked),
                CurrentAccess::Shared { .. } => (),
            }
        }

        Ok(())
    }

    pub(crate) unsafe fn cpu_read_lock(&mut self, range: Range<DeviceSize>) {
        self.ranges.split_at(&range.start);
        self.ranges.split_at(&range.end);

        for (_range, state) in self.ranges.range_mut(&range) {
            match &mut state.current_access {
                CurrentAccess::Shared { cpu_reads, .. } => {
                    *cpu_reads += 1;
                }
                _ => unreachable!("Buffer is being written by the CPU or GPU"),
            }
        }
    }

    pub(crate) unsafe fn cpu_read_unlock(&mut self, range: Range<DeviceSize>) {
        self.ranges.split_at(&range.start);
        self.ranges.split_at(&range.end);

        for (_range, state) in self.ranges.range_mut(&range) {
            match &mut state.current_access {
                CurrentAccess::Shared { cpu_reads, .. } => *cpu_reads -= 1,
                _ => unreachable!("Buffer was not locked for CPU read"),
            }
        }
    }

    pub(crate) fn check_cpu_write(
        &mut self,
        range: Range<DeviceSize>,
    ) -> Result<(), WriteLockError> {
        for (_range, state) in self.ranges.range(&range) {
            match &state.current_access {
                CurrentAccess::CpuExclusive => return Err(WriteLockError::CpuLocked),
                CurrentAccess::GpuExclusive { .. } => return Err(WriteLockError::GpuLocked),
                CurrentAccess::Shared {
                    cpu_reads: 0,
                    gpu_reads: 0,
                } => (),
                CurrentAccess::Shared { cpu_reads, .. } if *cpu_reads > 0 => {
                    return Err(WriteLockError::CpuLocked)
                }
                CurrentAccess::Shared { .. } => return Err(WriteLockError::GpuLocked),
            }
        }

        Ok(())
    }

    pub(crate) unsafe fn cpu_write_lock(&mut self, range: Range<DeviceSize>) {
        self.ranges.split_at(&range.start);
        self.ranges.split_at(&range.end);

        for (_range, state) in self.ranges.range_mut(&range) {
            state.current_access = CurrentAccess::CpuExclusive;
        }
    }

    pub(crate) unsafe fn cpu_write_unlock(&mut self, range: Range<DeviceSize>) {
        self.ranges.split_at(&range.start);
        self.ranges.split_at(&range.end);

        for (_range, state) in self.ranges.range_mut(&range) {
            match &mut state.current_access {
                CurrentAccess::CpuExclusive => {
                    state.current_access = CurrentAccess::Shared {
                        cpu_reads: 0,
                        gpu_reads: 0,
                    }
                }
                _ => unreachable!("Buffer was not locked for CPU write"),
            }
        }
    }

    pub(crate) fn check_gpu_read(&mut self, range: Range<DeviceSize>) -> Result<(), AccessError> {
        for (_range, state) in self.ranges.range(&range) {
            match &state.current_access {
                CurrentAccess::Shared { .. } => (),
                _ => return Err(AccessError::AlreadyInUse),
            }
        }

        Ok(())
    }

    pub(crate) unsafe fn gpu_read_lock(&mut self, range: Range<DeviceSize>) {
        self.ranges.split_at(&range.start);
        self.ranges.split_at(&range.end);

        for (_range, state) in self.ranges.range_mut(&range) {
            match &mut state.current_access {
                CurrentAccess::GpuExclusive { gpu_reads, .. }
                | CurrentAccess::Shared { gpu_reads, .. } => *gpu_reads += 1,
                _ => unreachable!("Buffer is being written by the CPU"),
            }
        }
    }

    pub(crate) unsafe fn gpu_read_unlock(&mut self, range: Range<DeviceSize>) {
        self.ranges.split_at(&range.start);
        self.ranges.split_at(&range.end);

        for (_range, state) in self.ranges.range_mut(&range) {
            match &mut state.current_access {
                CurrentAccess::GpuExclusive { gpu_reads, .. } => *gpu_reads -= 1,
                CurrentAccess::Shared { gpu_reads, .. } => *gpu_reads -= 1,
                _ => unreachable!("Buffer was not locked for GPU read"),
            }
        }
    }

    pub(crate) fn check_gpu_write(&mut self, range: Range<DeviceSize>) -> Result<(), AccessError> {
        for (_range, state) in self.ranges.range(&range) {
            match &state.current_access {
                CurrentAccess::Shared {
                    cpu_reads: 0,
                    gpu_reads: 0,
                } => (),
                _ => return Err(AccessError::AlreadyInUse),
            }
        }

        Ok(())
    }

    pub(crate) unsafe fn gpu_write_lock(&mut self, range: Range<DeviceSize>) {
        self.ranges.split_at(&range.start);
        self.ranges.split_at(&range.end);

        for (_range, state) in self.ranges.range_mut(&range) {
            match &mut state.current_access {
                CurrentAccess::GpuExclusive { gpu_writes, .. } => *gpu_writes += 1,
                &mut CurrentAccess::Shared {
                    cpu_reads: 0,
                    gpu_reads,
                } => {
                    state.current_access = CurrentAccess::GpuExclusive {
                        gpu_reads,
                        gpu_writes: 1,
                    }
                }
                _ => unreachable!("Buffer is being accessed by the CPU"),
            }
        }
    }

    pub(crate) unsafe fn gpu_write_unlock(&mut self, range: Range<DeviceSize>) {
        self.ranges.split_at(&range.start);
        self.ranges.split_at(&range.end);

        for (_range, state) in self.ranges.range_mut(&range) {
            match &mut state.current_access {
                &mut CurrentAccess::GpuExclusive {
                    gpu_reads,
                    gpu_writes: 1,
                } => {
                    state.current_access = CurrentAccess::Shared {
                        cpu_reads: 0,
                        gpu_reads,
                    }
                }
                CurrentAccess::GpuExclusive { gpu_writes, .. } => *gpu_writes -= 1,
                _ => unreachable!("Buffer was not locked for GPU write"),
            }
        }
    }
}

/// The current state of a specific range of bytes in a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BufferRangeState {
    current_access: CurrentAccess,
}

#[cfg(test)]
mod tests {
    use super::{
        BufferCreationError, BufferUsage, SparseLevel, UnsafeBuffer, UnsafeBufferCreateInfo,
    };
    use crate::{
        device::{Device, DeviceOwned},
        RequiresOneOf,
    };

    #[test]
    fn create() {
        let (device, _) = gfx_dev_and_queue!();
        let buf = UnsafeBuffer::new(
            device.clone(),
            UnsafeBufferCreateInfo {
                size: 128,
                usage: BufferUsage {
                    transfer_dst: true,
                    ..BufferUsage::empty()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let reqs = buf.memory_requirements();

        assert!(reqs.size >= 128);
        assert_eq!(buf.size(), 128);
        assert_eq!(&**buf.device() as *const Device, &*device as *const Device);
    }

    #[test]
    fn missing_feature_sparse_binding() {
        let (device, _) = gfx_dev_and_queue!();
        match UnsafeBuffer::new(
            device,
            UnsafeBufferCreateInfo {
                size: 128,
                sparse: Some(SparseLevel::empty()),
                usage: BufferUsage {
                    transfer_dst: true,
                    ..BufferUsage::empty()
                },
                ..Default::default()
            },
        ) {
            Err(BufferCreationError::RequirementNotMet {
                requires_one_of: RequiresOneOf { features, .. },
                ..
            }) if features.contains(&"sparse_binding") => (),
            _ => panic!(),
        }
    }

    #[test]
    fn missing_feature_sparse_residency() {
        let (device, _) = gfx_dev_and_queue!(sparse_binding);
        match UnsafeBuffer::new(
            device,
            UnsafeBufferCreateInfo {
                size: 128,
                sparse: Some(SparseLevel {
                    sparse_residency: true,
                    sparse_aliased: false,
                    ..Default::default()
                }),
                usage: BufferUsage {
                    transfer_dst: true,
                    ..BufferUsage::empty()
                },
                ..Default::default()
            },
        ) {
            Err(BufferCreationError::RequirementNotMet {
                requires_one_of: RequiresOneOf { features, .. },
                ..
            }) if features.contains(&"sparse_residency_buffer") => (),
            _ => panic!(),
        }
    }

    #[test]
    fn missing_feature_sparse_aliased() {
        let (device, _) = gfx_dev_and_queue!(sparse_binding);
        match UnsafeBuffer::new(
            device,
            UnsafeBufferCreateInfo {
                size: 128,
                sparse: Some(SparseLevel {
                    sparse_residency: false,
                    sparse_aliased: true,
                    ..Default::default()
                }),
                usage: BufferUsage {
                    transfer_dst: true,
                    ..BufferUsage::empty()
                },
                ..Default::default()
            },
        ) {
            Err(BufferCreationError::RequirementNotMet {
                requires_one_of: RequiresOneOf { features, .. },
                ..
            }) if features.contains(&"sparse_residency_aliased") => (),
            _ => panic!(),
        }
    }

    #[test]
    fn create_empty_buffer() {
        let (device, _) = gfx_dev_and_queue!();

        assert_should_panic!({
            UnsafeBuffer::new(
                device,
                UnsafeBufferCreateInfo {
                    size: 0,
                    usage: BufferUsage {
                        transfer_dst: true,
                        ..BufferUsage::empty()
                    },
                    ..Default::default()
                },
            )
        });
    }
}
