//use core::mem::size_of;
use std::borrow::{Cow};
use windows::Win32::System::Diagnostics::Etw::{ETW_BUFFER_CONTEXT, EVENT_HEADER, EVENT_RECORD, EVENT_HEADER_EXTENDED_DATA_ITEM};

pub enum EventRecord {
    Owned(EventRecordOwned),
    Raw(*const EVENT_RECORD),
    Serialized(Vec<u8>), // EventRecordSerialized as little endian bytes
}

#[derive(Clone)]
struct ExtendedDataItemOwned {
    ext_type: u16,
    flags: u16, // Bit 1: linkage, Bit 2-16: unused
    data: Vec<u8>,
}

#[derive(Clone)]
pub struct EventRecordOwned {
    event_header: EVENT_HEADER,
    buffer_context: ETW_BUFFER_CONTEXT,
    extended_data_items: Vec<ExtendedDataItemOwned>,
    user_data: Vec<u8>,
}

// Compatible with TDH
// #[repr(C)]
// struct EventRecordSerialized {
//     Size: u32,
//     EventHeader: EVENT_HEADER,
//     BufferContext: ETW_BUFFER_CONTEXT,
//     // data: [u8; Size - sizeof(self)]
// }

impl EventRecord {
    pub fn new(evt: *const EVENT_RECORD) -> Self {
        // TODO: Check pointers and lengths
        EventRecord::Raw(evt)
    }

    pub fn get_event_header(&self) -> Cow<EVENT_HEADER> {
        match self {
            EventRecord::Owned(evt) => Cow::Borrowed(&evt.event_header),
            EventRecord::Raw(evt) => unsafe {
                Cow::Borrowed(&(**evt).EventHeader)
            }
            EventRecord::Serialized(_evt) => {
                todo!()
            },
        }
    }

    pub fn get_buffer_context(&self) -> Cow<ETW_BUFFER_CONTEXT> {
        match self {
            EventRecord::Owned(evt) => Cow::Borrowed(&evt.buffer_context),
            EventRecord::Raw(evt) => unsafe {
                Cow::Borrowed(&(**evt).BufferContext)
            }
            EventRecord::Serialized(_evt) => {
                todo!()
            },
        }
    }

    pub fn get_user_data(&self) -> &[u8] {
        match self {
            EventRecord::Owned(evt) => evt.user_data.as_slice(),
            EventRecord::Raw(evt) => unsafe {
                core::slice::from_raw_parts((**evt).UserData as *const u8, (**evt).UserDataLength as usize)
            }
            EventRecord::Serialized(_evt) => {
                todo!()
            },
        }
    }

    pub fn get_extended_data_items(&self) -> impl Iterator<Item = ExtendedDataItem> {
        ExtendedDataItemIterator {
            evt: self,
            index: 0,
        }
    }

    // fn serialize(&self) -> EventRecordSerialized {
    //     let evt = match self {
    //         EventRecord::Borrowed(evt) => *evt,
    //         EventRecord::Serialized(_) => {
    //             panic!("event is already serialized");
    //         }
    //         EventRecord::Owned(_) => {
    //             todo!();
    //         }
    //     };

    //     unsafe {
    //         let required_size: usize = size_of::<EventRecordSerialized>() + ((*evt).ExtendedDataCount as usize * size_of::<ExtendedDataItemHeader>());
    //     }

    //     EventRecordSerialized { Size: 0, EventHeader: Default::default(), BufferContext: Default::default() }
    // }
}

// impl Clone for EventRecord {
//     fn clone(&self) -> Self {
//         match self {
//             EventRecord::Owned(evt) => EventRecord::Owned(evt.clone()),
//             EventRecord::Borrowed(_) => self.to_owned(),
//             EventRecord::Serialized(evt) => EventRecord::Serialized(evt.clone()),
//         }
//     }
// }

impl ToOwned for EventRecord {
    type Owned = EventRecord;

    fn to_owned(&self) -> <EventRecord as ToOwned>::Owned {
        match self {
            EventRecord::Owned(evt) => EventRecord::Owned(evt.clone()),
            EventRecord::Raw(evt) => {
                /*
                pub struct EVENT_RECORD {
                    pub EventHeader: EVENT_HEADER,
                    pub BufferContext: ETW_BUFFER_CONTEXT,
                    pub ExtendedDataCount: u16,
                    pub UserDataLength: u16,
                    pub ExtendedData: *mut EVENT_HEADER_EXTENDED_DATA_ITEM,
                    pub UserData: *mut ::core::ffi::c_void,
                    pub UserContext: *mut ::core::ffi::c_void,
                } */
                unsafe {
                    let evt = *evt;
                    let mut owned_exdi: Vec<ExtendedDataItemOwned> =
                        Vec::with_capacity((*evt).ExtendedDataCount as usize);

                    let raw_exdi = core::slice::from_raw_parts((*evt).ExtendedData, (*evt).ExtendedDataCount as usize);

                    for exdi in raw_exdi {
                        let mut data: Vec<u8> = Vec::with_capacity(exdi.DataSize as usize);
                        core::ptr::copy_nonoverlapping(exdi.DataPtr as *const u8, data.as_mut_ptr(), exdi.DataSize as usize);
                        data.set_len(exdi.DataSize as usize);

                        owned_exdi.push(ExtendedDataItemOwned {
                            ext_type: exdi.ExtType,
                            flags: exdi.Anonymous._bitfield,
                            data: data,
                        });
                    }

                    let mut user_data: Vec<u8> = Vec::with_capacity((*evt).UserDataLength as usize);
                    core::ptr::copy_nonoverlapping(
                        (*evt).UserData,
                        user_data.as_mut_ptr() as *mut core::ffi::c_void,
                        (*evt).UserDataLength as usize,
                    );
                    user_data.set_len((*evt).UserDataLength as usize);

                    let mut owned = EventRecordOwned {
                        event_header: (*evt).EventHeader,
                        buffer_context: (*evt).BufferContext,
                        extended_data_items: vec![],
                        user_data: user_data,
                    };

                    // Ensure flags are set properly
                    if owned.extended_data_items.is_empty() {
                        owned.event_header.Flags &= !1;
                    } else {
                        owned.event_header.Flags |= 1;
                    }

                    EventRecord::Owned(owned)
                }
            }
            EventRecord::Serialized(_s) => {
                todo!();
            }
        }
    }
}

pub struct ExtendedDataItemIterator<'a> {
    evt: &'a EventRecord,
    index: usize,
}

pub struct ExtendedDataItem<'a> {
    pub ext_type: u16,
    pub flags: u16,
    pub data: &'a [u8],
}

impl<'a> Iterator for ExtendedDataItemIterator<'a> {
    type Item = ExtendedDataItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.evt {
            EventRecord::Owned(evt) => {
                if self.index >= evt.extended_data_items.len() {
                    return None;
                }

                return Some(ExtendedDataItem {
                    ext_type: evt.extended_data_items[self.index].ext_type,
                    flags: evt.extended_data_items[self.index].flags,
                    data: evt.extended_data_items[self.index].data.as_slice(),
                });
            },
            EventRecord::Raw(evt) => unsafe {
                if self.index >= (**evt).ExtendedDataCount as usize {
                    return None;
                }

                let items = core::slice::from_raw_parts((**evt).ExtendedData as *const EVENT_HEADER_EXTENDED_DATA_ITEM, (**evt).ExtendedDataCount as usize);
                let exdi = &items[self.index];
                let exdi_data = core::slice::from_raw_parts(exdi.DataPtr as *const u8, exdi.DataSize as usize);
                return Some(ExtendedDataItem {
                    ext_type: items[self.index].ExtType,
                    flags: items[self.index].Anonymous._bitfield,
                    data: exdi_data,
                });
            }
            EventRecord::Serialized(_evt) => {
                todo!()
            },
        };
    }
}

// // Compatible with TDH
// #[repr(C)]
// struct ExtendedDataItemHeader {
//     Reserved1: u16,
//     ExtType: u16,
//     Reserved2: u16, // Bit 1: Linkage, Bits 2-16: Reserved2
//     DataSize: u16,
// }
