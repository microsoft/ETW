use std::borrow::Cow;
use windows::Win32::System::Diagnostics::Etw::{
    ETW_BUFFER_CONTEXT, EVENT_HEADER, EVENT_HEADER_EXTENDED_DATA_ITEM,
    EVENT_HEADER_FLAG_EXTENDED_INFO, EVENT_RECORD,
};

/// An ETW event, either borrowed from the ETW callback, owned, or serialized to bytes.
pub enum EventRecord<'a> {
    /// A deep copy of an event; see [`EventRecord::into_owned`].
    Owned(EventRecordOwned),
    /// An event borrowed from the ETW callback.
    Raw(&'a EVENT_RECORD),
    /// An event in a self-contained byte format; see [`EventRecord::serialize`] for the layout.
    ///
    /// Construct this with [`EventRecord::from_serialized`] to validate the bytes; the
    /// accessors panic if the bytes are malformed.
    Serialized(Vec<u8>),
}

#[derive(Clone)]
struct ExtendedDataItemOwned {
    reserved1: u16,
    ext_type: u16,
    flags: u16, // Bit 1: linkage, Bit 2-16: unused
    data: Vec<u8>,
}

/// A deep copy of an ETW event that does not borrow from the ETW callback.
#[derive(Clone)]
pub struct EventRecordOwned {
    event_header: EVENT_HEADER,
    buffer_context: ETW_BUFFER_CONTEXT,
    extended_data_items: Vec<ExtendedDataItemOwned>,
    user_data: Vec<u8>,
}

// Serialized layout, compatible with TdhEx1SerializeEventRecord / TdhEx1DeserializeEventRecord
// (TDH_SERIALIZED_EVENT_RECORD in tdhEx1.h). The header structs are copied in their Windows
// in-memory layout, which has no padding and is little-endian on every Windows target.
//
//   u32 Size                    Total length of the serialized event in bytes
//   ETW_BUFFER_CONTEXT          4 bytes
//   EVENT_HEADER                80 bytes
//   Extended data items         Present iff EventHeader.Flags has EVENT_HEADER_FLAG_EXTENDED_INFO.
//                               Each is the first 8 bytes of EVENT_HEADER_EXTENDED_DATA_ITEM:
//                                 u16 Reserved1, u16 ExtType,
//                                 u16 Linkage:1 (another item follows) | Reserved2:15,
//                                 u16 DataSize,
//                               then DataSize bytes of data, zero-padded to a multiple of 8.
//   User data                   The remaining bytes (at most 65535)
#[cfg(target_endian = "big")]
compile_error!("EventRecord serialization assumes a little-endian target");

const SERIALIZED_BUFFER_CONTEXT_OFFSET: usize = size_of::<u32>();
const SERIALIZED_HEADER_OFFSET: usize = SERIALIZED_BUFFER_CONTEXT_OFFSET + size_of::<ETW_BUFFER_CONTEXT>();
const SERIALIZED_PREFIX_LEN: usize = SERIALIZED_HEADER_OFFSET + size_of::<EVENT_HEADER>();
const EXT_ITEM_HEADER_LEN: usize = 4 * size_of::<u16>();
const EXT_ITEM_LINKAGE: u16 = 1;
const EXT_ITEM_ALIGNMENT: usize = 8;

// Copying these as bytes is only sound if they have no padding
const _: () = assert!(size_of::<EVENT_HEADER>() == 80);
const _: () = assert!(size_of::<ETW_BUFFER_CONTEXT>() == 4);
const _: () = assert!(SERIALIZED_PREFIX_LEN == 88);

fn struct_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: only used with the padding-free plain-data ETW structs asserted above
    unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn padded_ext_data_len(data_size: usize) -> usize {
    data_size.next_multiple_of(EXT_ITEM_ALIGNMENT)
}

/// Validates serialized bytes, returning the extended data item region and the user data.
/// Applies the same checks as TdhEx1DeserializeEventRecord.
fn parse_serialized(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    if bytes.len() < SERIALIZED_PREFIX_LEN {
        return None;
    }
    let size = u32::from_le_bytes(bytes[..SERIALIZED_BUFFER_CONTEXT_OFFSET].try_into().ok()?);
    if size as usize != bytes.len() {
        return None;
    }

    let header = serialized_event_header(bytes);
    let mut ext_end = SERIALIZED_PREFIX_LEN;
    if header.Flags & EVENT_HEADER_FLAG_EXTENDED_INFO as u16 != 0 {
        let mut item_count = 0usize;
        loop {
            if bytes.len() - ext_end < EXT_ITEM_HEADER_LEN {
                return None;
            }
            item_count += 1;
            if item_count > u16::MAX as usize {
                return None;
            }
            let flags = read_u16(bytes, ext_end + 4);
            let padded_size = padded_ext_data_len(read_u16(bytes, ext_end + 6) as usize);
            ext_end += EXT_ITEM_HEADER_LEN;
            if bytes.len() - ext_end < padded_size {
                return None;
            }
            ext_end += padded_size;
            if flags & EXT_ITEM_LINKAGE == 0 {
                break;
            }
        }
    }

    let user_data = &bytes[ext_end..];
    if user_data.len() > u16::MAX as usize {
        return None;
    }

    Some((&bytes[SERIALIZED_PREFIX_LEN..ext_end], user_data))
}

fn parse_serialized_or_panic(bytes: &[u8]) -> (&[u8], &[u8]) {
    parse_serialized(bytes)
        .expect("malformed serialized event; construct it with EventRecord::from_serialized")
}

fn serialized_event_header(bytes: &[u8]) -> EVENT_HEADER {
    let header = &bytes[SERIALIZED_HEADER_OFFSET..SERIALIZED_PREFIX_LEN];
    // SAFETY: the slice is exactly size_of::<EVENT_HEADER>() bytes, and any bit pattern is valid
    unsafe { core::ptr::read_unaligned(header.as_ptr() as *const EVENT_HEADER) }
}

fn serialized_buffer_context(bytes: &[u8]) -> ETW_BUFFER_CONTEXT {
    let context = &bytes[SERIALIZED_BUFFER_CONTEXT_OFFSET..SERIALIZED_HEADER_OFFSET];
    // SAFETY: the slice is exactly size_of::<ETW_BUFFER_CONTEXT>() bytes, and any bit pattern is valid
    unsafe { core::ptr::read_unaligned(context.as_ptr() as *const ETW_BUFFER_CONTEXT) }
}

impl<'a> EventRecord<'a> {
    /// Wraps a borrowed `EVENT_RECORD` without copying it.
    pub fn from_ref(evt: &'a EVENT_RECORD) -> Self {
        // TODO: Check pointers and lengths
        EventRecord::Raw(evt)
    }

    /// Wraps a raw `EVENT_RECORD` pointer without copying it.
    ///
    /// Returns `E_UNEXPECTED` if `evt` is null or misaligned.
    ///
    /// # Safety
    ///
    /// If non-null and aligned, `evt` must point to a valid `EVENT_RECORD` whose header,
    /// user data and extended data buffers stay valid and unmodified for the caller-chosen lifetime `'a`.
    pub unsafe fn from_ptr(evt: *const EVENT_RECORD) -> Result<Self, windows::core::Error> {
        if evt.is_null() || !evt.is_aligned() {
            Err(crate::error::E_UNEXPECTED.into()) //TODO
        }
        else {
            Ok(EventRecord::<'static>::Raw(unsafe { &*evt } ))
        }
    }

    /// Returns the event header (provider, descriptor, timestamp, process/thread IDs, ...).
    ///
    /// # Panics
    ///
    /// Panics if an [`EventRecord::Serialized`] event is malformed.
    pub fn get_event_header(&self) -> Cow<'_, EVENT_HEADER> {
        match self {
            EventRecord::Owned(evt) => Cow::Borrowed(&evt.event_header),
            EventRecord::Raw(evt) => Cow::Borrowed(&evt.EventHeader),
            EventRecord::Serialized(bytes) => {
                parse_serialized_or_panic(bytes);
                Cow::Owned(serialized_event_header(bytes))
            }
        }
    }

    /// Returns the buffer context (processor number and logger ID).
    ///
    /// # Panics
    ///
    /// Panics if an [`EventRecord::Serialized`] event is malformed.
    pub fn get_buffer_context(&self) -> Cow<'_, ETW_BUFFER_CONTEXT> {
        match self {
            EventRecord::Owned(evt) => Cow::Borrowed(&evt.buffer_context),
            EventRecord::Raw(evt) => Cow::Borrowed(&evt.BufferContext),
            EventRecord::Serialized(bytes) => {
                parse_serialized_or_panic(bytes);
                Cow::Owned(serialized_buffer_context(bytes))
            }
        }
    }

    /// Copies any data borrowed from the ETW callback so the event can be kept
    /// after the consumer has moved on to the next event.
    pub fn into_owned(self) -> EventRecord<'static> {
        match self {
            EventRecord::Owned(evt) => EventRecord::Owned(evt),
            EventRecord::Raw(evt) => EventRecord::Owned(EventRecordOwned::from_raw(evt)),
            EventRecord::Serialized(evt) => EventRecord::Serialized(evt),
        }
    }

    /// Returns the event's payload bytes.
    ///
    /// # Panics
    ///
    /// Panics if an [`EventRecord::Serialized`] event is malformed.
    pub fn get_user_data(&self) -> &[u8] {
        match self {
            EventRecord::Owned(evt) => evt.user_data.as_slice(),
            EventRecord::Raw(evt) => unsafe {
                raw_slice(evt.UserData as *const u8, evt.UserDataLength as usize)
            },
            EventRecord::Serialized(bytes) => parse_serialized_or_panic(bytes).1,
        }
    }

    /// Iterates over the event's extended data items (e.g. related activity ID, stack trace,
    /// TraceLogging schema).
    ///
    /// # Panics
    ///
    /// Panics if an [`EventRecord::Serialized`] event is malformed.
    pub fn get_extended_data_items(&self) -> impl Iterator<Item = ExtendedDataItem<'_>> {
        self.extended_data_item_iter()
    }

    fn extended_data_item_iter(&self) -> ExtendedDataItemIterator<'_, 'a> {
        let serialized_items = match self {
            EventRecord::Serialized(bytes) => parse_serialized_or_panic(bytes).0,
            _ => &[],
        };
        ExtendedDataItemIterator {
            evt: self,
            index: 0,
            serialized_items,
        }
    }

    /// Validates `bytes` as a serialized event (see [`serialize`](Self::serialize)) and wraps it.
    ///
    /// Returns `HRESULT_FROM_WIN32(ERROR_INVALID_DATA)` if `bytes` is malformed.
    pub fn from_serialized(bytes: Vec<u8>) -> Result<EventRecord<'static>, windows::core::Error> {
        if parse_serialized(&bytes).is_none() {
            return Err(crate::error::E_INVALID_DATA.into());
        }
        Ok(EventRecord::Serialized(bytes))
    }

    /// Serializes the event into a single self-contained byte buffer that can be stored or
    /// sent elsewhere and read back with [`from_serialized`](Self::from_serialized).
    ///
    /// The format is `TDH_SERIALIZED_EVENT_RECORD` from `tdhEx1.h`, so the bytes are
    /// interchangeable with `TdhEx1SerializeEventRecord` / `TdhEx1DeserializeEventRecord`.
    /// Layout, with integers in little-endian order:
    ///
    /// | Field | Size |
    /// |-------|------|
    /// | `Size`: total size of the serialized event in bytes | `u32` |
    /// | `ETW_BUFFER_CONTEXT`, in its Windows in-memory layout | 4 bytes |
    /// | `EVENT_HEADER`, in its Windows in-memory layout | 80 bytes |
    /// | Extended data items, present iff `EventHeader.Flags` has `EVENT_HEADER_FLAG_EXTENDED_INFO`. Each is the first 8 bytes of `EVENT_HEADER_EXTENDED_DATA_ITEM` (`Reserved1`, `ExtType`, `Linkage:1`/`Reserved2:15`, `DataSize`; `Linkage` is set if another item follows), then `DataSize` bytes of data zero-padded to a multiple of 8 | variable |
    /// | User data | the remaining bytes (at most 65535) |
    ///
    /// # Panics
    ///
    /// Panics if an [`EventRecord::Serialized`] event is malformed.
    pub fn serialize(&self) -> Vec<u8> {
        if let EventRecord::Serialized(bytes) = self {
            parse_serialized_or_panic(bytes);
            return bytes.clone();
        }

        let mut items_iter = self.extended_data_item_iter();
        let items: Vec<(u16, ExtendedDataItem)> =
            core::iter::from_fn(|| items_iter.next_with_reserved1()).collect();
        let user_data = self.get_user_data();
        let size = SERIALIZED_PREFIX_LEN
            + items
                .iter()
                .map(|(_, item)| EXT_ITEM_HEADER_LEN + padded_ext_data_len(item.data.len()))
                .sum::<usize>()
            + user_data.len();

        let mut header = self.get_event_header().into_owned();
        if items.is_empty() {
            header.Flags &= !(EVENT_HEADER_FLAG_EXTENDED_INFO as u16);
        } else {
            header.Flags |= EVENT_HEADER_FLAG_EXTENDED_INFO as u16;
        }

        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(&u32::try_from(size).expect("event too large").to_le_bytes());
        bytes.extend_from_slice(struct_bytes(&*self.get_buffer_context()));
        bytes.extend_from_slice(struct_bytes(&header));
        for (i, (reserved1, item)) in items.iter().enumerate() {
            let flags = if i + 1 < items.len() {
                item.flags | EXT_ITEM_LINKAGE
            } else {
                item.flags & !EXT_ITEM_LINKAGE
            };
            // ETW limits extended data items to u16 sizes, so this only fails on a logic error
            let data_size = u16::try_from(item.data.len()).expect("extended data item too large");

            bytes.extend_from_slice(&reserved1.to_le_bytes());
            bytes.extend_from_slice(&item.ext_type.to_le_bytes());
            bytes.extend_from_slice(&flags.to_le_bytes());
            bytes.extend_from_slice(&data_size.to_le_bytes());
            bytes.extend_from_slice(item.data);
            bytes.resize(bytes.len() + padded_ext_data_len(item.data.len()) - item.data.len(), 0);
        }
        bytes.extend_from_slice(user_data);
        debug_assert_eq!(bytes.len(), size);

        bytes
    }
}

impl<'a> ToOwned for EventRecord<'a> {
    type Owned = EventRecord<'a>;

    fn to_owned(&self) -> <EventRecord<'a> as ToOwned>::Owned {
        match self {
            EventRecord::Owned(evt) => EventRecord::Owned(evt.clone()),
            EventRecord::Raw(evt) => EventRecord::Owned(EventRecordOwned::from_raw(evt)),
            EventRecord::Serialized(evt) => EventRecord::Serialized(evt.clone()),
        }
    }
}

impl EventRecordOwned {
    fn from_raw(evt: &EVENT_RECORD) -> Self {
        let raw_exdi = unsafe {
            raw_slice(
                evt.ExtendedData as *const EVENT_HEADER_EXTENDED_DATA_ITEM,
                evt.ExtendedDataCount as usize,
            )
        };

        let extended_data_items: Vec<ExtendedDataItemOwned> = raw_exdi
            .iter()
            .map(|exdi| ExtendedDataItemOwned {
                reserved1: exdi.Reserved1,
                ext_type: exdi.ExtType,
                flags: exdi.Anonymous._bitfield,
                data: unsafe { raw_slice(exdi.DataPtr as *const u8, exdi.DataSize as usize) }
                    .to_vec(),
            })
            .collect();

        let user_data =
            unsafe { raw_slice(evt.UserData as *const u8, evt.UserDataLength as usize) }.to_vec();

        let mut event_header = evt.EventHeader;
        if extended_data_items.is_empty() {
            event_header.Flags &= !(EVENT_HEADER_FLAG_EXTENDED_INFO as u16);
        } else {
            event_header.Flags |= EVENT_HEADER_FLAG_EXTENDED_INFO as u16;
        }

        EventRecordOwned {
            event_header,
            buffer_context: evt.BufferContext,
            extended_data_items,
            user_data,
        }
    }
}

/// Like `slice::from_raw_parts`, but tolerates the null pointers ETW uses for empty buffers.
unsafe fn raw_slice<'s, T>(ptr: *const T, len: usize) -> &'s [T] {
    if len == 0 || ptr.is_null() {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }
}

/// Iterator returned by [`EventRecord::get_extended_data_items`].
pub struct ExtendedDataItemIterator<'a, 'evt> {
    evt: &'a EventRecord<'evt>,
    // Owned/Raw: index of the next item
    index: usize,
    // Serialized: the not-yet-iterated part of the validated extended data item region
    serialized_items: &'a [u8],
}

/// An extended data item attached to an event.
pub struct ExtendedDataItem<'a> {
    /// The `EVENT_HEADER_EXT_TYPE_*` value.
    pub ext_type: u16,
    /// Bit 0 is the linkage bit (set if another item follows); the rest are reserved.
    pub flags: u16,
    /// The item's data.
    pub data: &'a [u8],
}

impl<'a, 'evt> Iterator for ExtendedDataItemIterator<'a, 'evt> {
    type Item = ExtendedDataItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_with_reserved1().map(|(_, item)| item)
    }
}

impl<'a, 'evt> ExtendedDataItemIterator<'a, 'evt> {
    // Also returns the item's Reserved1 field, which `serialize` preserves
    fn next_with_reserved1(&mut self) -> Option<(u16, ExtendedDataItem<'a>)> {
        match self.evt {
            EventRecord::Owned(evt) => {
                if self.index >= evt.extended_data_items.len() {
                    return None;
                }

                let exdi = &evt.extended_data_items[self.index];
                self.index += 1;

                Some((exdi.reserved1, ExtendedDataItem {
                    ext_type: exdi.ext_type,
                    flags: exdi.flags,
                    data: exdi.data.as_slice(),
                }))
            }
            EventRecord::Raw(evt) => unsafe {
                if self.index >= evt.ExtendedDataCount as usize {
                    return None;
                }

                let items = core::slice::from_raw_parts(
                    evt.ExtendedData as *const EVENT_HEADER_EXTENDED_DATA_ITEM,
                    evt.ExtendedDataCount as usize,
                );
                let exdi = &items[self.index];
                self.index += 1;
                let exdi_data = raw_slice(exdi.DataPtr as *const u8, exdi.DataSize as usize);

                Some((exdi.Reserved1, ExtendedDataItem {
                    ext_type: exdi.ExtType,
                    flags: exdi.Anonymous._bitfield,
                    data: exdi_data,
                }))
            },
            EventRecord::Serialized(_) => {
                let items = self.serialized_items;
                if items.is_empty() {
                    return None;
                }

                // `parse_serialized` already checked that every item header and its data are in bounds
                let reserved1 = read_u16(items, 0);
                let ext_type = read_u16(items, 2);
                let flags = read_u16(items, 4);
                let data_size = read_u16(items, 6) as usize;
                let padded = &items[EXT_ITEM_HEADER_LEN..];
                let data = &padded[..data_size];
                self.serialized_items = &padded[padded_ext_data_len(data_size)..];

                Some((reserved1, ExtendedDataItem {
                    ext_type,
                    flags,
                    data,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use windows::core::GUID;

    use super::*;

    fn ext_item(reserved1: u16, ext_type: u16, linkage: bool, data: &[u8]) -> EVENT_HEADER_EXTENDED_DATA_ITEM {
        let mut item = EVENT_HEADER_EXTENDED_DATA_ITEM {
            Reserved1: reserved1,
            ExtType: ext_type,
            DataSize: data.len() as u16,
            DataPtr: data.as_ptr() as u64,
            ..Default::default()
        };
        item.Anonymous._bitfield = linkage as u16;
        item
    }

    fn raw_record(ext: &mut [EVENT_HEADER_EXTENDED_DATA_ITEM], user_data: &mut [u8]) -> EVENT_RECORD {
        let mut record = EVENT_RECORD::default();
        record.EventHeader.ProviderId = GUID::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        record.EventHeader.ThreadId = 42;
        record.EventHeader.ProcessId = 7;
        record.EventHeader.TimeStamp = 0x1122_3344_5566_7788;
        record.EventHeader.EventDescriptor.Id = 5;
        record.EventHeader.EventDescriptor.Keyword = 0x8000_0000_0000_0001;
        if !ext.is_empty() {
            record.EventHeader.Flags = EVENT_HEADER_FLAG_EXTENDED_INFO as u16;
        }
        record.BufferContext.LoggerId = 3;
        record.ExtendedDataCount = ext.len() as u16;
        record.ExtendedData = ext.as_mut_ptr();
        record.UserDataLength = user_data.len() as u16;
        record.UserData = user_data.as_mut_ptr().cast();
        record
    }

    fn assert_same_event(a: &EventRecord, b: &EventRecord) {
        assert_eq!(struct_bytes(&*a.get_event_header()), struct_bytes(&*b.get_event_header()));
        assert_eq!(struct_bytes(&*a.get_buffer_context()), struct_bytes(&*b.get_buffer_context()));
        assert_eq!(a.get_user_data(), b.get_user_data());
        let items = |evt: &EventRecord| {
            let mut iter = evt.extended_data_item_iter();
            core::iter::from_fn(|| iter.next_with_reserved1())
                .map(|(reserved1, i)| (reserved1, i.ext_type, i.flags, i.data.to_vec()))
                .collect::<Vec<_>>()
        };
        assert_eq!(items(a), items(b));
    }

    #[test]
    fn serialize_round_trip() -> Result<(), windows::core::Error> {
        let (data1, data2) = ([0xAAu8; 16], [0xBBu8; 3]);
        let mut ext = [ext_item(0x1234, 1, true, &data1), ext_item(0x5678, 11, false, &data2)];
        let mut user_data = [1u8, 2, 3, 4, 5];
        let record = raw_record(&mut ext, &mut user_data);
        let raw = EventRecord::from_ref(&record);

        let bytes = raw.serialize();
        // The 3-byte item is zero-padded to 8 bytes
        assert_eq!(bytes.len(), SERIALIZED_PREFIX_LEN + (8 + 16) + (8 + 8) + 5);
        assert_eq!(&bytes[SERIALIZED_PREFIX_LEN + 24 + 8..][..8], [0xBB, 0xBB, 0xBB, 0, 0, 0, 0, 0]);
        // TDH_SERIALIZED_EVENT_RECORD: Size, BufferContext, EventHeader
        assert_eq!(&bytes[..4], (bytes.len() as u32).to_le_bytes());
        assert_eq!(&bytes[4..8], struct_bytes(&record.BufferContext));
        assert_eq!(&bytes[8..88], struct_bytes(&record.EventHeader));
        // Owned and Raw records serialize identically
        assert_eq!(raw.to_owned().serialize(), bytes);

        let serialized = EventRecord::from_serialized(bytes.clone())?;
        assert_same_event(&raw, &serialized);
        assert_eq!(serialized.get_extended_data_items().map(|i| i.flags).collect::<Vec<_>>(), [1, 0]);
        assert_eq!(serialized.get_event_header().EventDescriptor.Keyword, 0x8000_0000_0000_0001);
        assert_eq!(serialized.serialize(), bytes);

        Ok(())
    }

    #[test]
    fn serialize_round_trip_without_extended_data_or_user_data() -> Result<(), windows::core::Error> {
        let record = raw_record(&mut [], &mut []);
        let raw = EventRecord::from_ref(&record);

        let bytes = raw.serialize();
        assert_eq!(bytes.len(), SERIALIZED_PREFIX_LEN);

        let serialized = EventRecord::from_serialized(bytes)?;
        assert_same_event(&raw, &serialized);
        assert_eq!(serialized.get_extended_data_items().count(), 0);
        assert!(serialized.get_user_data().is_empty());

        Ok(())
    }

    #[test]
    fn from_serialized_rejects_malformed_bytes() {
        let data = [0xAAu8; 4];
        let mut ext = [ext_item(0, 1, false, &data)];
        let record = raw_record(&mut ext, &mut []);
        let bytes = EventRecord::from_ref(&record).serialize();

        let with_size = |mut b: Vec<u8>| {
            let len = b.len() as u32;
            b[..4].copy_from_slice(&len.to_le_bytes());
            b
        };
        // Cuts into the item's padding, which TdhEx1DeserializeEventRecord requires
        let truncated_padding = with_size(bytes[..bytes.len() - 1].to_vec());
        let wrong_size = {
            let mut b = bytes.clone();
            b.push(0);
            b
        };
        let missing_item = with_size(bytes[..SERIALIZED_PREFIX_LEN].to_vec());
        let too_much_user_data = {
            let record = raw_record(&mut [], &mut []);
            let mut b = EventRecord::from_ref(&record).serialize();
            b.resize(b.len() + u16::MAX as usize + 1, 0);
            with_size(b)
        };

        for malformed in [bytes[..10].to_vec(), truncated_padding, wrong_size, missing_item, too_much_user_data] {
            let err = EventRecord::from_serialized(malformed).err().expect("malformed bytes were accepted");
            assert_eq!(err.code(), crate::error::E_INVALID_DATA);
        }
    }

    #[test]
    #[should_panic(expected = "malformed serialized event")]
    fn malformed_serialized_variant_panics() {
        EventRecord::Serialized(vec![0; 10]).get_user_data();
    }
}