//! Provides the functionality for memory management across devices.
//!
//! A SharedMemory tracks the memory copies across the devices of the Backend and manages
//!
//! * the location of these memory copies
//! * the location of the latest memory copy and
//! * the synchronisation of memory copies between devices
//!
//! A [memory copy][mem] represents one logical unit of data, which might me located at the host. The
//! SharedMemory, tracks the location of the data blob across the various devices that the backend might
//! consist of. This allows us to run operations on various backends with the same data blob.
//!
//! [frameworks]: ../frameworks/index.html
//! [mem]: ../memory/index.html
//!
//! ## Examples
//!
//! Create SharedMemory and fill it with some numbers:
//!
//! ```
//! #![feature(clone_from_slice)]
//! # extern crate collenchyma;
//! use collenchyma::framework::IFramework;
//! use collenchyma::frameworks::Native;
//! use collenchyma::shared_memory::SharedMemory;
//! # fn main() {
//! // allocate memory
//! let native = Native::new();
//! let device = native.new_device(native.hardwares()).unwrap();
//! let shared_data = &mut SharedMemory::<i32>::new(&device, 5).unwrap();
//! // fill memory with some numbers
//! let local_data = [0, 1, 2, 3, 4];
//! let data = shared_data.get_mut(&device).unwrap().as_mut_native().unwrap();
//! data.as_mut_slice().clone_from_slice(&local_data);
//! # }
//! ```

use linear_map::LinearMap;
use device::{IDevice, DeviceType};
use memory::MemoryType;
use std::marker::PhantomData;
use std::{fmt, mem, error};

// #[derive(Debug)]
/// Container that handles synchronization of [Memory][1] of type `T`.
/// [1]: ../memory/index.html
#[allow(missing_debug_implementations)] // due to LinearMap
pub struct SharedMemory<T> {
    latest_location: DeviceType,
    latest_copy: MemoryType,
    copies: LinearMap<DeviceType, MemoryType>,
    cap: usize,
    phantom: PhantomData<T>,
}

impl<T> SharedMemory<T> {
    /// Create new SharedMemory by allocating [Memory][1] on a Device.
    /// [1]: ../memory/index.html
    pub fn new(dev: &DeviceType, capacity: usize) -> Result<SharedMemory<T>, Error> {
        let copies = LinearMap::<DeviceType, MemoryType>::new();
        let copy: MemoryType;
        let alloc_size = Self::mem_size(capacity);
        match *dev {
            DeviceType::Native(ref cpu) => copy = MemoryType::Native(try!(cpu.alloc_memory(alloc_size as u64))),
            DeviceType::OpenCL(ref context) => copy = MemoryType::OpenCL(try!(context.alloc_memory(alloc_size as u64))),
            #[cfg(feature = "cuda")]
            DeviceType::Cuda(ref context) => copy = MemoryType::Cuda(try!(context.alloc_memory(alloc_size as u64))),
        }
        Ok(SharedMemory {
            latest_location: dev.clone(),
            latest_copy: copy,
            copies: copies,
            cap: capacity,
            phantom: PhantomData,
        })
    }

    /// Synchronize memory from latest location to `destination`.
    pub fn sync(&mut self, destination: &DeviceType) -> Result<(), Error> {
        if &self.latest_location != destination {
            let latest = self.latest_location.clone();
            try!(self.sync_from_to(&latest, &destination));
            self.latest_location = destination.clone();
            self.latest_copy = try!(self.copies.remove(destination).ok_or(Error::MissingDestination("SharedMemory does not hold a copy on destination device.")));
        }
        Ok(())
    }

    /// Get a reference to the memory copy on the provided `device`.
    ///
    /// Returns `None` if there is no memory copy on the device.
    pub fn get(&self, device: &DeviceType) -> Option<&MemoryType> {
        // first check if device is not current location. This is cheaper than a lookup in `copies`.
        if &self.latest_location == device {
            return Some(&self.latest_copy)
        }
        self.copies.get(device)
    }

    /// Get a mutable reference to the memory copy on the provided `device`.
    ///
    /// Returns `None` if there is no memory copy on the device.
    pub fn get_mut(&mut self, device: &DeviceType) -> Option<&mut MemoryType> {
        // first check if device is not current location. This is cheaper than a lookup in `copies`.
        if &self.latest_location == device {
            return Some(&mut self.latest_copy)
        }
        self.copies.get_mut(device)
    }

    /// Synchronize memory from `source` device to `destination` device.
    fn sync_from_to(&mut self, source: &DeviceType, destination: &DeviceType) -> Result<(), Error> {
        if source != destination {
            match self.aquire_copies(source, destination) {
                Ok((mut source_copy, mut destination_copy)) => {
                    match destination {
                        &DeviceType::Native(ref cpu) => {
                            match destination_copy.as_mut_native() {
                                Some(ref mut mem) => try!(cpu.sync_in(source, &source_copy, mem)),
                                None => return Err(Error::InvalidMemory("Expected Native Memory (FlatBox)"))
                            }
                        },
                        &DeviceType::OpenCL(ref context) => unimplemented!(),
                        #[cfg(feature = "cuda")]
                        &DeviceType::Cuda(ref context) => {
                            match destination_copy.as_mut_cuda() {
                                Some(ref mut mem) => try!(context.sync_in(source, &source_copy, mem)),
                                None => return Err(Error::InvalidMemory("Expected CUDA Memory."))
                            }
                        }
                    }
                    self.return_copies(source, source_copy, destination, destination_copy);
                    Ok(())
                },
                Err(err) => Err(err),
            }
        } else {
            Ok(())
        }
    }

    /// Aquire ownership over the copies for synchronizing.
    fn aquire_copies(&mut self, source: &DeviceType, destination: &DeviceType) -> Result<(MemoryType, MemoryType), Error> {
        let source_copy: MemoryType;
        let destination_copy: MemoryType;
        match self.copies.remove(source) {
            Some(source_cpy) => source_copy = source_cpy,
            None => return Err(Error::MissingSource("SharedMemory does not hold a copy on source device."))
        }
        match self.copies.remove(destination) {
            Some(destination_cpy) => destination_copy = destination_cpy,
            None => return Err(Error::MissingDestination("SharedMemory does not hold a copy on destination device."))
        }

        Ok((source_copy, destination_copy))
    }

    /// Return ownership over the copies after synchronizing.
    fn return_copies(&mut self, src: &DeviceType, src_mem: MemoryType, dest: &DeviceType, dest_mem: MemoryType) {
        self.copies.insert(src.clone(), src_mem);
        self.copies.insert(dest.clone(), dest_mem);
    }

    /// Track a new `device` and allocate memory on it.
    ///
    /// Returns an error if the SharedMemory is already tracking the `device`.
    pub fn add_device(&mut self, device: &DeviceType) -> Result<&mut Self, Error> {
        // first check if device is not current location. This is cheaper than a lookup in `copies`.
        if &self.latest_location == device {
            return Err(Error::InvalidMemoryAllocation("SharedMemory already tracks memory for this device. No memory allocation."))
        }
        match self.copies.get(device) {
            Some(_) => Err(Error::InvalidMemoryAllocation("SharedMemory already tracks memory for this device. No memory allocation.")),
            None => {
                let copy: MemoryType;
                match *device {
                    DeviceType::Native(ref cpu) => copy = MemoryType::Native(try!(cpu.alloc_memory(Self::mem_size(self.capacity()) as u64))),
                    DeviceType::OpenCL(ref context) => copy = MemoryType::OpenCL(try!(context.alloc_memory(Self::mem_size(self.capacity()) as u64))),
                    #[cfg(feature = "cuda")]
                    DeviceType::Cuda(ref context) => copy = MemoryType::Cuda(try!(context.alloc_memory(Self::mem_size(self.capacity()) as u64))),
                };
                self.copies.insert(device.clone(), copy);
                Ok(self)
            }
        }
    }

    /// Returns the device that contains the up-to-date memory copy.
    pub fn latest_device(&self) -> &DeviceType {
        &self.latest_location
    }

    /// Returns the number of elements for which the SharedMemory has been allocated.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    fn mem_size(capacity: usize) -> usize {
        mem::size_of::<T>() * capacity
    }
}

/// Errors than can occur when synchronizing memory.
#[derive(Debug, Copy, Clone)]
pub enum Error {
    /// No copy on source device.
    MissingSource(&'static str),
    /// No copy on destination device.
    MissingDestination(&'static str),
    /// No valid MemoryType provided. Other than expected.
    InvalidMemory(&'static str),
    /// No memory allocation on specified device happened.
    InvalidMemoryAllocation(&'static str),
    /// Framework error at memory allocation.
    MemoryAllocationError(::device::Error),
    /// Framework error at memory synchronization.
    MemorySynchronizationError(::device::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::MissingSource(ref err) => write!(f, "{:?}", err),
            Error::MissingDestination(ref err) => write!(f, "{:?}", err),
            Error::InvalidMemory(ref err) => write!(f, "{:?}", err),
            Error::InvalidMemoryAllocation(ref err) => write!(f, "{:?}", err),
            Error::MemoryAllocationError(ref err) => write!(f, "{}", err),
            Error::MemorySynchronizationError(ref err) => write!(f, "{}", err),
        }
    }
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::MissingSource(ref err) => err,
            Error::MissingDestination(ref err) => err,
            Error::InvalidMemory(ref err) => err,
            Error::InvalidMemoryAllocation(ref err) => err,
            Error::MemoryAllocationError(ref err) => err.description(),
            Error::MemorySynchronizationError(ref err) => err.description(),
        }
    }

    fn cause(&self) -> Option<&error::Error> {
        match *self {
            Error::MissingSource(_) => None,
            Error::MissingDestination(_) => None,
            Error::InvalidMemory(_) => None,
            Error::InvalidMemoryAllocation(_) => None,
            Error::MemoryAllocationError(ref err) => Some(err),
            Error::MemorySynchronizationError(ref err) => Some(err),
        }
    }
}

impl From<Error> for ::error::Error {
    fn from(err: Error) -> ::error::Error {
        ::error::Error::SharedMemory(err)
    }
}
