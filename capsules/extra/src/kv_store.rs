// Licensed under the Apache License, Version 2.0 or the MIT License.
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Copyright Tock Contributors 2022.

//! Tock Key-Value store capsule.
//!
//! This capsule provides a virtualized Key-Value store interface based on an
//! underlying `hil::kv_system` storage layer.
//!
//! ```
//! +-----------------------+
//! |                       |
//! |  Capsule using K-V    |
//! |                       |
//! +-----------------------+
//!
//!    capsules::kv_store
//!
//! +-----------------------+
//! |                       |
//! | K-V store (this file) |
//! |                       |
//! +-----------------------+
//!
//!    hil::kv_system
//!
//! +-----------------------+
//! |                       |
//! |  K-V library          |
//! |                       |
//! +-----------------------+
//!
//!    hil::flash
//! ```

use core::mem;
// use kernel::collections::list::{List, ListLink, ListNode};
use kernel::hil::kv_system::{self, KVSystem};
use kernel::storage_permissions::StoragePermissions;
use kernel::utilities::cells::{MapCell, OptionalCell, TakeCell};
use kernel::utilities::leasable_buffer::SubSliceMut;
use kernel::ErrorCode;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Operation {
    Get,
    Set,
    Delete,
}

/// Current version of the Tock K-V header.
const HEADER_VERSION: u8 = 0;
pub const HEADER_LENGTH: usize = mem::size_of::<KeyHeader>();

/// This is the header used for KV stores.
#[repr(packed)]
struct KeyHeader {
    version: u8,
    length: u32,
    write_id: u32,
}

impl KeyHeader {
    /// Create a new `KeyHeader` from a buffer
    fn new_from_buf(buf: &[u8]) -> Self {
        Self {
            version: buf[0],
            length: u32::from_le_bytes(buf[1..5].try_into().unwrap_or([0; 4])),
            write_id: u32::from_le_bytes(buf[5..9].try_into().unwrap_or([0; 4])),
        }
    }

    /// Copy the header to `buf`
    fn copy_to_buf(&self, buf: &mut [u8]) {
        buf[0] = self.version;
        buf[1..5].copy_from_slice(&self.length.to_le_bytes());
        buf[5..9].copy_from_slice(&self.write_id.to_le_bytes());
    }
}

/// Implement this trait and use `set_client()` in order to receive callbacks.
pub trait StoreClient {
    /// This callback is called when the get operation completes.
    ///
    /// - `result`: Nothing on success, 'ErrorCode' on error
    /// - `key`: The key buffer
    /// - `ret_buf`: The ret_buf buffer
    fn get_complete(
        &self,
        result: Result<(), ErrorCode>,
        unhashed_key: SubSliceMut<'static, u8>,
        value: SubSliceMut<'static, u8>,
    );

    /// This callback is called when the set operation completes.
    ///
    /// - `result`: Nothing on success, 'ErrorCode' on error
    /// - `key`: The key buffer
    /// - `value`: The value buffer
    fn set_complete(
        &self,
        result: Result<(), ErrorCode>,
        unhashed_key: SubSliceMut<'static, u8>,
        value: SubSliceMut<'static, u8>,
    );

    /// This callback is called when the delete operation completes.
    ///
    /// - `result`: Nothing on success, 'ErrorCode' on error
    /// - `key`: The key buffer
    fn delete_complete(
        &self,
        result: Result<(), ErrorCode>,
        unhashed_key: SubSliceMut<'static, u8>,
    );
}

/// High-level Key-Value interface with permissions.
///
/// This interface provides access to key-value storage where access control.
/// Each object is marked with a `write_id` (based on the `StoragePermissions`
/// used to create it), and all further accesses and modifications to that
/// object require suitable permissions.
pub trait KV<'a> {
    /// Configure the client for operation callbacks.
    fn set_client(&self, client: &'a dyn StoreClient);

    /// Retrieve a value based on the given key.
    ///
    /// ### Arguments
    ///
    /// - `key`: The key to identify the k-v pair. Unhashed.
    /// - `value`: Where the returned value buffer will be stored.
    /// - `permissions`: The read/write/modify permissions for this access.
    ///
    /// ### Return
    /// - On success returns `Ok(())`.
    /// - On error, returns the buffers and:
    ///   - `ENOSUPPORT`: The key could not be found.
    ///   - `SIZE`: The value is longer than the provided buffer. The amount of
    ///     the value that fits in the buffer will be provided.
    fn get(
        &self,
        key: LeasableMutableBuffer<'static, u8>,
        value: LeasableMutableBuffer<'static, u8>,
        permissions: StoragePermissions,
    ) -> Result<
        (),
        (
            LeasableMutableBuffer<'static, u8>,
            LeasableMutableBuffer<'static, u8>,
            Result<(), ErrorCode>,
        ),
    >;

    /// Store a value based on the given key.
    ///
    /// The `value` buffer must have room for a header.
    ///
    /// ### Arguments
    ///
    /// - `key`: The key to identify the k-v pair. Unhashed.
    /// - `value`: The value to store. The provided buffer MUST start
    ///   `KV.header_size()` bytes after the beginning of the buffer to enable
    ///   the implementation to insert a header.
    /// - `permissions`: The read/write/modify permissions for this access.
    fn set(
        &self,
        key: LeasableMutableBuffer<'static, u8>,
        value: LeasableMutableBuffer<'static, u8>,
        permissions: StoragePermissions,
    ) -> Result<
        (),
        (
            LeasableMutableBuffer<'static, u8>,
            LeasableMutableBuffer<'static, u8>,
            Result<(), ErrorCode>,
        ),
    >;

    /// Delete a key-value object based on the given key.
    ///
    /// ### Arguments
    ///
    /// - `key`: The key to identify the k-v pair. Unhashed.
    /// - `permissions`: The read/write/modify permissions for this access.
    fn delete(
        &self,
        key: LeasableMutableBuffer<'static, u8>,
        permissions: StoragePermissions,
    ) -> Result<(), (LeasableMutableBuffer<'static, u8>, Result<(), ErrorCode>)>;

    /// Returns the length of the key-value store's header in bytes.
    ///
    /// Room for this header must be accommodated in a `set` operation.
    fn header_size(&self) -> usize;
}

pub struct KVStore<'a, K: KVSystem<'a> + KVSystem<'a, K = T>, T: 'static + kv_system::KeyType> {
    // mux_kv: &'a MuxKVStore<'a, K, T>,
    // next: ListLink<'a, KVStore<'a, K, T>>,
    kv: &'a K,
    hashed_key: TakeCell<'static, T>,
    header_value: TakeCell<'static, [u8]>,

    client: OptionalCell<&'a dyn StoreClient>,
    operation: OptionalCell<Operation>,

    unhashed_key: MapCell<SubSliceMut<'static, u8>>,
    value: MapCell<SubSliceMut<'static, u8>>,
    valid_ids: OptionalCell<StoragePermissions>,
}

// impl<'a, K: KVSystem<'a, K = T>, T: kv_system::KeyType> ListNode<'a, KVStore<'a, K, T>>
//     for KVStore<'a, K, T>
// {
//     fn next(&self) -> &'a ListLink<KVStore<'a, K, T>> {
//         &self.next
//     }
// }

impl<'a, K: KVSystem<'a, K = T>, T: kv_system::KeyType> KVStore<'a, K, T> {
    // pub fn new(mux_kv: &'a MuxKVStore<'a, K, T>) -> KVStore<'a, K, T> {
    //     Self {
    //         mux_kv,
    //         next: ListLink::empty(),
    //         client: OptionalCell::empty(),
    //         operation: OptionalCell::empty(),
    //         unhashed_key: MapCell::empty(),
    //         value: MapCell::empty(),
    //         valid_ids: OptionalCell::empty(),
    //     }
    // }

    pub fn new(
        kv: &'a K,
        key: &'static mut T,
        header_value: &'static mut [u8; HEADER_LENGTH],
    ) -> KVStore<'a, K, T> {
        Self {
            kv,
            hashed_key: TakeCell::new(key),
            header_value: TakeCell::new(header_value),
            client: OptionalCell::empty(),
            operation: OptionalCell::empty(),
            unhashed_key: MapCell::empty(),
            value: MapCell::empty(),
            valid_ids: OptionalCell::empty(),
        }
    }

    // pub fn setup(&'a self) {
    //     self.mux_kv.users.push_head(self);
    // }
}

impl<'a, K: KVSystem<'a, K = T>, T: kv_system::KeyType> KV<'a> for KVStore<'a, K, T> {
    fn set_client(&self, client: &'a dyn StoreClient) {
        self.client.set(client);
    }

    fn get(
        &self,
        key: SubSliceMut<'static, u8>,
        value: SubSliceMut<'static, u8>,
        permissions: StoragePermissions,
    ) -> Result<
        (),
        (
            SubSliceMut<'static, u8>,
            SubSliceMut<'static, u8>,
            Result<(), ErrorCode>,
        ),
    > {
        if self.operation.is_some() {
            return Err((key, value, Err(ErrorCode::BUSY)));
        }

        self.operation.set(Operation::Get);
        self.valid_ids.set(permissions);
        self.value.replace(value);

        self.hashed_key
            .take()
            .map_or(Err(ErrorCode::FAIL), |hashed_key| {
                match self.kv.generate_key(key, hashed_key) {
                    Ok(()) => Ok(()),
                    Err((unhashed_key, hashed_key, e)) => {
                        self.operation.clear();
                        self.hashed_key.replace(hashed_key);
                        self.unhashed_key.replace(unhashed_key);
                        e
                    }
                }
            })
            .map_err(|e| {
                (
                    self.unhashed_key.take().unwrap(),
                    self.value.take().unwrap(),
                    Err(e),
                )
            })
    }

    fn set(
        &self,
        key: SubSliceMut<'static, u8>,
        mut value: SubSliceMut<'static, u8>,
        permissions: StoragePermissions,
    ) -> Result<
        (),
        (
            SubSliceMut<'static, u8>,
            SubSliceMut<'static, u8>,
            Result<(), ErrorCode>,
        ),
    > {
        let write_id = match permissions.get_write_id() {
            Some(write_id) => write_id,
            None => return Err((key, value, Err(ErrorCode::INVAL))),
        };

        if self.operation.is_some() {
            return Err((key, value, Err(ErrorCode::BUSY)));
        }

        // The caller must ensure there is space for the header.
        if value.len() < HEADER_LENGTH {
            return Err((key, value, Err(ErrorCode::SIZE)));
        }

        // Create the Tock header.
        let header = KeyHeader {
            version: HEADER_VERSION,
            length: (value.len() - HEADER_LENGTH) as u32,
            write_id,
        };

        // Copy in the header to the buffer.
        header.copy_to_buf(value.as_slice());

        self.operation.set(Operation::Set);
        self.valid_ids.set(permissions);
        // self.unhashed_key.replace(key);
        self.value.replace(value);
        // self.start_operation();
        // Ok(())

        // self.start_operation(false).map_err(|e| {
        //     (
        //         self.unhashed_key.take().unwrap(),
        //         self.value.take().unwrap(),
        //         e,
        //     )
        // })

        self.hashed_key
            .take()
            .map_or(Err(ErrorCode::FAIL), |hashed_key| {
                match self.kv.generate_key(key, hashed_key) {
                    Ok(()) => Ok(()),
                    Err((unhashed_key, hashed_key, e)) => {
                        self.operation.clear();
                        self.hashed_key.replace(hashed_key);
                        self.unhashed_key.replace(unhashed_key);
                        e
                    }
                }
            })
            .map_err(|e| {
                (
                    self.unhashed_key.take().unwrap(),
                    self.value.take().unwrap(),
                    Err(e),
                )
            })
    }

    fn delete(
        &self,
        key: SubSliceMut<'static, u8>,
        permissions: StoragePermissions,
    ) -> Result<(), (SubSliceMut<'static, u8>, Result<(), ErrorCode>)> {
        if self.operation.is_some() {
            return Err((key, Err(ErrorCode::BUSY)));
        }

        self.operation.set(Operation::Delete);
        self.valid_ids.set(permissions);
        // self.unhashed_key.replace(key);
        // self.start_operation();
        // Ok(())

        // self.start_operation(false)
        //     .map_err(|e| (self.unhashed_key.take().unwrap(), e))

        self.hashed_key
            .take()
            .map_or(Err(ErrorCode::FAIL), |hashed_key| {
                match self.kv.generate_key(key, hashed_key) {
                    Ok(()) => Ok(()),
                    Err((unhashed_key, hashed_key, e)) => {
                        self.hashed_key.replace(hashed_key);
                        self.operation.clear();
                        self.unhashed_key.replace(unhashed_key);
                        e
                    }
                }
            })
            .map_err(|e| (self.unhashed_key.take().unwrap(), Err(e)))
    }

    fn header_size(&self) -> usize {
        HEADER_LENGTH
    }
}

// /// Keep track of whether the kv is busy with doing a cleanup.
// #[derive(PartialEq)]
// enum StateCleanup {
//     CleanupRequested,
//     CleanupInProgress,
// }

// pub struct MuxKVStore<'a, K: KVSystem<'a> + KVSystem<'a, K = T>, T: 'static + kv_system::KeyType> {

//     cleanup: OptionalCell<StateCleanup>,
//     users: List<'a, KVStore<'a, K, T>>,
//     inflight: OptionalCell<&'a KVStore<'a, K, T>>,
// }

// impl<'a, K: KVSystem<'a> + KVSystem<'a, K = T>, T: 'static + kv_system::KeyType>
//     MuxKVStore<'a, K, T>
// {
//     pub fn new(
//         kv: &'a K,
//         key: &'static mut T,
//         header_value: &'static mut [u8; HEADER_LENGTH],
//     ) -> MuxKVStore<'a, K, T> {
//         Self {
//             kv,
//             hashed_key: TakeCell::new(key),
//             header_value: TakeCell::new(header_value),
//             inflight: OptionalCell::empty(),
//             cleanup: OptionalCell::empty(),
//             users: List::new(),
//         }
//     }

// }

impl<'a, K: KVSystem<'a, K = T>, T: kv_system::KeyType> kv_system::Client<T> for KVStore<'a, K, T> {
    fn generate_key_complete(
        &self,
        result: Result<(), ErrorCode>,
        unhashed_key: SubSliceMut<'static, u8>,
        hashed_key: &'static mut T,
    ) {
        self.operation.map(|op| {
            if result.is_err() {
                // On error, we re-store our state, run the next pending
                // operation, and notify the original user that their
                // operation failed using a callback.
                self.hashed_key.replace(hashed_key);
                self.operation.clear();

                match op {
                    Operation::Get => {
                        self.value.take().map(|value| {
                            self.client.map(move |cb| {
                                cb.get_complete(result, unhashed_key, value);
                            });
                        });
                    }
                    Operation::Set => {
                        self.value.take().map(|value| {
                            self.client.map(move |cb| {
                                cb.set_complete(result, unhashed_key, value);
                            });
                        });
                    }
                    Operation::Delete => {
                        self.client.map(move |cb| {
                            cb.delete_complete(result, unhashed_key);
                        });
                    }
                }
                // });
            } else {
                match op {
                    Operation::Get => {
                        self.value
                            .take()
                            .map(|value| match self.kv.get_value(hashed_key, value) {
                                Ok(()) => {
                                    self.unhashed_key.replace(unhashed_key);
                                }
                                Err((key, value, e)) => {
                                    self.hashed_key.replace(key);
                                    self.operation.clear();
                                    self.client.map(move |cb| {
                                        cb.get_complete(e, unhashed_key, value);
                                    });
                                }
                            });
                    }
                    Operation::Set => {
                        self.value.take().map(|value| {
                            match self.kv.append_key(hashed_key, value) {
                                Ok(()) => {
                                    self.unhashed_key.replace(unhashed_key);
                                }
                                Err((key, value, e)) => {
                                    self.hashed_key.replace(key);
                                    self.operation.clear();
                                    self.client.map(move |cb| {
                                        cb.set_complete(e, unhashed_key, value);
                                    });
                                }
                            }
                        });
                    }
                    Operation::Delete => {
                        self.header_value.take().map(|value| {
                            match self
                                .kv
                                .get_value(hashed_key, LeasableMutableBuffer::new(value))
                            {
                                Ok(()) => {
                                    self.unhashed_key.replace(unhashed_key);
                                }
                                Err((key, value, e)) => {
                                    self.hashed_key.replace(key);
                                    self.header_value.replace(value.take());
                                    self.operation.clear();
                                    self.client.map(move |cb| {
                                        cb.delete_complete(e, unhashed_key);
                                    });
                                }
                            }
                        });
                    }
                }
            }
        });
    }

    fn append_key_complete(
        &self,
        result: Result<(), ErrorCode>,
        key: &'static mut T,
        value: SubSliceMut<'static, u8>,
    ) {
        self.hashed_key.replace(key);

        self.operation.map(|op| match op {
            Operation::Get | Operation::Delete => {}
            Operation::Set => {
                match result {
                    Err(ErrorCode::NOSUPPORT) => {
                        // We could not append because of a collision. So
                        // now we must figure out if we are allowed to
                        // overwrite this key. That starts by reading the
                        // key.
                        self.hashed_key.take().map(|hashed_key| {
                            self.header_value.take().map(|header_value| {
                                match self
                                    .kv
                                    .get_value(hashed_key, LeasableMutableBuffer::new(header_value))
                                {
                                    Ok(()) => {
                                        self.value.replace(value);
                                    }
                                    Err((key, hvalue, e)) => {
                                        self.hashed_key.replace(key);
                                        self.header_value.replace(hvalue.take());
                                        self.operation.clear();
                                        self.unhashed_key.take().map(|unhashed_key| {
                                            self.client.map(move |cb| {
                                                cb.set_complete(e, unhashed_key, value);
                                            });
                                        });
                                    }
                                }
                            });
                        });
                    }
                    _ => {
                        // On success or any other error we just return the
                        // result back to the caller via a callback.
                        self.operation.clear();
                        self.unhashed_key.take().map(|unhashed_key| {
                            self.client.map(move |cb| {
                                cb.set_complete(result, unhashed_key, value);
                            });
                        });
                    }
                }
            }
        });
    }

    fn get_value_complete(
        &self,
        result: Result<(), ErrorCode>,
        key: &'static mut T,
        mut ret_buf: SubSliceMut<'static, u8>,
    ) {
        self.operation.map(|op| {
            match op {
                Operation::Set => {
                    // If we get here, we must have been trying to append
                    // the key but ran in to a collision. Now that we have
                    // retrieved the existing key, we can check if we are
                    // allowed to overwrite this key.
                    let mut access_allowed = false;

                    if result.is_ok() || result.err() == Some(ErrorCode::SIZE) {
                        let header = KeyHeader::new_from_buf(ret_buf.as_slice());

                        if header.version == HEADER_VERSION {
                            self.valid_ids.map(|perms| {
                                access_allowed = perms.check_write_permission(header.write_id);
                            });
                        }
                    }

                    self.header_value.replace(ret_buf.take());

                    if access_allowed {
                        match self.kv.invalidate_key(key) {
                            Ok(()) => {}

                            Err((key, e)) => {
                                self.operation.clear();
                                self.hashed_key.replace(key);
                                self.unhashed_key.take().map(|unhashed_key| {
                                    self.value.take().map(|value| {
                                        self.client.map(move |cb| {
                                            cb.set_complete(e, unhashed_key, value);
                                        });
                                    });
                                });
                            }
                        }
                    } else {
                        self.operation.clear();
                        self.hashed_key.replace(key);
                        self.unhashed_key.take().map(|unhashed_key| {
                            self.value.take().map(|value| {
                                self.client.map(move |cb| {
                                    cb.set_complete(Err(ErrorCode::FAIL), unhashed_key, value);
                                });
                            });
                        });
                    }
                }
                Operation::Delete => {
                    let mut access_allowed = false;

                    // Before we delete an object we retrieve the header to
                    // ensure that we have permissions to access it. In that
                    // case we don't need to supply a buffer long enough to
                    // store the full value, so a `SIZE` error code is ok
                    // and we can continue to remove the object.
                    if result.is_ok() || result.err() == Some(ErrorCode::SIZE) {
                        let header = KeyHeader::new_from_buf(ret_buf.as_slice());

                        if header.version == HEADER_VERSION {
                            self.valid_ids.map(|perms| {
                                access_allowed = perms.check_write_permission(header.write_id);
                            });
                        }
                    }

                    self.header_value.replace(ret_buf.take());

                    if access_allowed {
                        match self.kv.invalidate_key(key) {
                            Ok(()) => {}

                            Err((key, e)) => {
                                self.operation.clear();
                                self.hashed_key.replace(key);
                                self.unhashed_key.take().map(|unhashed_key| {
                                    self.client.map(move |cb| {
                                        cb.delete_complete(e, unhashed_key);
                                    });
                                });
                            }
                        }
                    } else {
                        self.operation.clear();
                        self.hashed_key.replace(key);
                        self.unhashed_key.take().map(|unhashed_key| {
                            self.client.map(move |cb| {
                                cb.delete_complete(Err(ErrorCode::FAIL), unhashed_key);
                            });
                        });
                    }
                }
                Operation::Get => {
                    self.hashed_key.replace(key);
                    self.operation.clear();

                    let mut read_allowed = false;

                    if result.is_ok() || result.err() == Some(ErrorCode::SIZE) {
                        let header = KeyHeader::new_from_buf(ret_buf.as_slice());

                        if header.version == HEADER_VERSION {
                            self.valid_ids.map(|perms| {
                                read_allowed = perms.check_read_permission(header.write_id);
                            });

                            if read_allowed {
                                // Remove the header from the accessible
                                // portion of the buffer.
                                ret_buf.slice(HEADER_LENGTH..);
                            }
                        }
                    }

                    if !read_allowed {
                        // Access denied or the header is invalid, zero the buffer.
                        ret_buf.as_slice().iter_mut().for_each(|m| *m = 0)
                    }

                    self.unhashed_key.take().map(|unhashed_key| {
                        self.client.map(move |cb| {
                            if read_allowed {
                                cb.get_complete(result, unhashed_key, ret_buf);
                            } else {
                                // The operation failed or the caller
                                // doesn't have permission, just return the
                                // error for key not found (and an empty
                                // buffer).
                                cb.get_complete(Err(ErrorCode::NOSUPPORT), unhashed_key, ret_buf);
                            }
                        });
                    });
                }
            }
        });
    }

    fn invalidate_key_complete(&self, result: Result<(), ErrorCode>, key: &'static mut T) {
        self.hashed_key.replace(key);

        self.operation.map(|op| match op {
            Operation::Get => {}
            Operation::Set => {
                // Now that we have deleted the existing key-value we can
                // store our new key and value.
                match result {
                    Ok(()) => {
                        self.hashed_key.take().map(|hashed_key| {
                            self.value.take().map(|value| {
                                match self.kv.append_key(hashed_key, value) {
                                    Ok(()) => {}
                                    Err((key, value, e)) => {
                                        self.hashed_key.replace(key);
                                        self.operation.clear();
                                        self.unhashed_key.take().map(|unhashed_key| {
                                            self.client.map(move |cb| {
                                                cb.set_complete(e, unhashed_key, value);
                                            });
                                        });
                                    }
                                }
                            });
                        });
                    }
                    _ => {
                        // Some error with delete, signal error.
                        self.operation.clear();
                        self.unhashed_key.take().map(|unhashed_key| {
                            self.value.take().map(|value| {
                                self.client.map(move |cb| {
                                    cb.set_complete(Err(ErrorCode::NOSUPPORT), unhashed_key, value);
                                });
                            });
                        });
                    }
                }
            }
            Operation::Delete => {
                self.operation.clear();
                self.unhashed_key.take().map(|unhashed_key| {
                    self.client.map(move |cb| {
                        cb.delete_complete(result, unhashed_key);
                    });
                });
            }
        });

        // self.cleanup.set(StateCleanup::CleanupRequested);
        // self.start_operation();
    }

    fn garbage_collect_complete(&self, _result: Result<(), ErrorCode>) {
        // self.cleanup.clear();
    }
}
