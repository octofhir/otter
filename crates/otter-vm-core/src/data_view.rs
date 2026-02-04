//! DataView implementation
//!
//! DataView provides a low-level interface for reading and writing multiple
//! number types in an ArrayBuffer, with control over byte order (endianness).

use crate::array_buffer::JsArrayBuffer;
use crate::gc::GcRef;
use std::sync::Arc;

/// A JavaScript DataView
///
/// DataView provides arbitrary access to an ArrayBuffer with explicit
/// control over byte ordering (little-endian or big-endian).
#[derive(Debug)]
pub struct JsDataView {
    /// The underlying ArrayBuffer
    buffer: GcRef<JsArrayBuffer>,
    /// Byte offset into the buffer
    byte_offset: usize,
    /// Length of the view in bytes
    byte_length: usize,
}

impl otter_vm_gc::GcTraceable for JsDataView {
    const NEEDS_TRACE: bool = true;
    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace the buffer's object field
        tracer(self.buffer.object.header() as *const _);
    }
}

impl JsDataView {
    /// Create a new DataView over an ArrayBuffer
    pub fn new(
        buffer: GcRef<JsArrayBuffer>,
        byte_offset: usize,
        byte_length: Option<usize>,
    ) -> Result<Self, &'static str> {
        if buffer.is_detached() {
            return Err("cannot create DataView on detached buffer");
        }

        let buf_len = buffer.byte_length();

        if byte_offset > buf_len {
            return Err("byte offset is outside the bounds of the buffer");
        }

        let actual_length = match byte_length {
            Some(len) => {
                if byte_offset + len > buf_len {
                    return Err("byte length extends beyond the buffer");
                }
                len
            }
            None => buf_len - byte_offset,
        };

        Ok(Self {
            buffer,
            byte_offset,
            byte_length: actual_length,
        })
    }

    /// Get the underlying ArrayBuffer
    pub fn buffer(&self) -> GcRef<JsArrayBuffer> {
        self.buffer
    }

    /// Get the byte offset into the buffer
    pub fn byte_offset(&self) -> usize {
        self.byte_offset
    }

    /// Get the byte length of the view
    pub fn byte_length(&self) -> usize {
        if self.buffer.is_detached() {
            0
        } else {
            self.byte_length
        }
    }

    /// Check if the underlying buffer is detached
    pub fn is_detached(&self) -> bool {
        self.buffer.is_detached()
    }

    // Helper to check bounds
    fn check_bounds(&self, byte_offset: usize, size: usize) -> Result<(), &'static str> {
        if self.buffer.is_detached() {
            return Err("ArrayBuffer is detached");
        }
        if byte_offset + size > self.byte_length {
            return Err("offset is outside the bounds of the DataView");
        }
        Ok(())
    }

    // ===== Getters =====

    /// Get an Int8 at the specified byte offset
    pub fn get_int8(&self, byte_offset: usize) -> Result<i8, &'static str> {
        self.check_bounds(byte_offset, 1)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| data[abs_offset] as i8)
            .ok_or("ArrayBuffer is detached")
    }

    /// Get a Uint8 at the specified byte offset
    pub fn get_uint8(&self, byte_offset: usize) -> Result<u8, &'static str> {
        self.check_bounds(byte_offset, 1)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| data[abs_offset])
            .ok_or("ArrayBuffer is detached")
    }

    /// Get an Int16 at the specified byte offset
    pub fn get_int16(&self, byte_offset: usize, little_endian: bool) -> Result<i16, &'static str> {
        self.check_bounds(byte_offset, 2)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| {
                let bytes = [data[abs_offset], data[abs_offset + 1]];
                if little_endian {
                    i16::from_le_bytes(bytes)
                } else {
                    i16::from_be_bytes(bytes)
                }
            })
            .ok_or("ArrayBuffer is detached")
    }

    /// Get a Uint16 at the specified byte offset
    pub fn get_uint16(&self, byte_offset: usize, little_endian: bool) -> Result<u16, &'static str> {
        self.check_bounds(byte_offset, 2)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| {
                let bytes = [data[abs_offset], data[abs_offset + 1]];
                if little_endian {
                    u16::from_le_bytes(bytes)
                } else {
                    u16::from_be_bytes(bytes)
                }
            })
            .ok_or("ArrayBuffer is detached")
    }

    /// Get an Int32 at the specified byte offset
    pub fn get_int32(&self, byte_offset: usize, little_endian: bool) -> Result<i32, &'static str> {
        self.check_bounds(byte_offset, 4)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| {
                let bytes = [
                    data[abs_offset],
                    data[abs_offset + 1],
                    data[abs_offset + 2],
                    data[abs_offset + 3],
                ];
                if little_endian {
                    i32::from_le_bytes(bytes)
                } else {
                    i32::from_be_bytes(bytes)
                }
            })
            .ok_or("ArrayBuffer is detached")
    }

    /// Get a Uint32 at the specified byte offset
    pub fn get_uint32(&self, byte_offset: usize, little_endian: bool) -> Result<u32, &'static str> {
        self.check_bounds(byte_offset, 4)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| {
                let bytes = [
                    data[abs_offset],
                    data[abs_offset + 1],
                    data[abs_offset + 2],
                    data[abs_offset + 3],
                ];
                if little_endian {
                    u32::from_le_bytes(bytes)
                } else {
                    u32::from_be_bytes(bytes)
                }
            })
            .ok_or("ArrayBuffer is detached")
    }

    /// Get a Float32 at the specified byte offset
    pub fn get_float32(
        &self,
        byte_offset: usize,
        little_endian: bool,
    ) -> Result<f32, &'static str> {
        self.check_bounds(byte_offset, 4)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| {
                let bytes = [
                    data[abs_offset],
                    data[abs_offset + 1],
                    data[abs_offset + 2],
                    data[abs_offset + 3],
                ];
                if little_endian {
                    f32::from_le_bytes(bytes)
                } else {
                    f32::from_be_bytes(bytes)
                }
            })
            .ok_or("ArrayBuffer is detached")
    }

    /// Get a Float64 at the specified byte offset
    pub fn get_float64(
        &self,
        byte_offset: usize,
        little_endian: bool,
    ) -> Result<f64, &'static str> {
        self.check_bounds(byte_offset, 8)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| {
                let bytes = [
                    data[abs_offset],
                    data[abs_offset + 1],
                    data[abs_offset + 2],
                    data[abs_offset + 3],
                    data[abs_offset + 4],
                    data[abs_offset + 5],
                    data[abs_offset + 6],
                    data[abs_offset + 7],
                ];
                if little_endian {
                    f64::from_le_bytes(bytes)
                } else {
                    f64::from_be_bytes(bytes)
                }
            })
            .ok_or("ArrayBuffer is detached")
    }

    /// Get a BigInt64 at the specified byte offset
    pub fn get_big_int64(
        &self,
        byte_offset: usize,
        little_endian: bool,
    ) -> Result<i64, &'static str> {
        self.check_bounds(byte_offset, 8)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| {
                let bytes = [
                    data[abs_offset],
                    data[abs_offset + 1],
                    data[abs_offset + 2],
                    data[abs_offset + 3],
                    data[abs_offset + 4],
                    data[abs_offset + 5],
                    data[abs_offset + 6],
                    data[abs_offset + 7],
                ];
                if little_endian {
                    i64::from_le_bytes(bytes)
                } else {
                    i64::from_be_bytes(bytes)
                }
            })
            .ok_or("ArrayBuffer is detached")
    }

    /// Get a BigUint64 at the specified byte offset
    pub fn get_big_uint64(
        &self,
        byte_offset: usize,
        little_endian: bool,
    ) -> Result<u64, &'static str> {
        self.check_bounds(byte_offset, 8)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data(|data| {
                let bytes = [
                    data[abs_offset],
                    data[abs_offset + 1],
                    data[abs_offset + 2],
                    data[abs_offset + 3],
                    data[abs_offset + 4],
                    data[abs_offset + 5],
                    data[abs_offset + 6],
                    data[abs_offset + 7],
                ];
                if little_endian {
                    u64::from_le_bytes(bytes)
                } else {
                    u64::from_be_bytes(bytes)
                }
            })
            .ok_or("ArrayBuffer is detached")
    }

    // ===== Setters =====

    /// Set an Int8 at the specified byte offset
    pub fn set_int8(&self, byte_offset: usize, value: i8) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 1)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer
            .with_data_mut(|data| data[abs_offset] = value as u8);
        Ok(())
    }

    /// Set a Uint8 at the specified byte offset
    pub fn set_uint8(&self, byte_offset: usize, value: u8) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 1)?;
        let abs_offset = self.byte_offset + byte_offset;
        self.buffer.with_data_mut(|data| data[abs_offset] = value);
        Ok(())
    }

    /// Set an Int16 at the specified byte offset
    pub fn set_int16(
        &self,
        byte_offset: usize,
        value: i16,
        little_endian: bool,
    ) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 2)?;
        let abs_offset = self.byte_offset + byte_offset;
        let bytes = if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        self.buffer.with_data_mut(|data| {
            data[abs_offset] = bytes[0];
            data[abs_offset + 1] = bytes[1];
        });
        Ok(())
    }

    /// Set a Uint16 at the specified byte offset
    pub fn set_uint16(
        &self,
        byte_offset: usize,
        value: u16,
        little_endian: bool,
    ) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 2)?;
        let abs_offset = self.byte_offset + byte_offset;
        let bytes = if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        self.buffer.with_data_mut(|data| {
            data[abs_offset] = bytes[0];
            data[abs_offset + 1] = bytes[1];
        });
        Ok(())
    }

    /// Set an Int32 at the specified byte offset
    pub fn set_int32(
        &self,
        byte_offset: usize,
        value: i32,
        little_endian: bool,
    ) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 4)?;
        let abs_offset = self.byte_offset + byte_offset;
        let bytes = if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        self.buffer.with_data_mut(|data| {
            data[abs_offset..abs_offset + 4].copy_from_slice(&bytes);
        });
        Ok(())
    }

    /// Set a Uint32 at the specified byte offset
    pub fn set_uint32(
        &self,
        byte_offset: usize,
        value: u32,
        little_endian: bool,
    ) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 4)?;
        let abs_offset = self.byte_offset + byte_offset;
        let bytes = if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        self.buffer.with_data_mut(|data| {
            data[abs_offset..abs_offset + 4].copy_from_slice(&bytes);
        });
        Ok(())
    }

    /// Set a Float32 at the specified byte offset
    pub fn set_float32(
        &self,
        byte_offset: usize,
        value: f32,
        little_endian: bool,
    ) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 4)?;
        let abs_offset = self.byte_offset + byte_offset;
        let bytes = if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        self.buffer.with_data_mut(|data| {
            data[abs_offset..abs_offset + 4].copy_from_slice(&bytes);
        });
        Ok(())
    }

    /// Set a Float64 at the specified byte offset
    pub fn set_float64(
        &self,
        byte_offset: usize,
        value: f64,
        little_endian: bool,
    ) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 8)?;
        let abs_offset = self.byte_offset + byte_offset;
        let bytes = if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        self.buffer.with_data_mut(|data| {
            data[abs_offset..abs_offset + 8].copy_from_slice(&bytes);
        });
        Ok(())
    }

    /// Set a BigInt64 at the specified byte offset
    pub fn set_big_int64(
        &self,
        byte_offset: usize,
        value: i64,
        little_endian: bool,
    ) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 8)?;
        let abs_offset = self.byte_offset + byte_offset;
        let bytes = if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        self.buffer.with_data_mut(|data| {
            data[abs_offset..abs_offset + 8].copy_from_slice(&bytes);
        });
        Ok(())
    }

    /// Set a BigUint64 at the specified byte offset
    pub fn set_big_uint64(
        &self,
        byte_offset: usize,
        value: u64,
        little_endian: bool,
    ) -> Result<(), &'static str> {
        self.check_bounds(byte_offset, 8)?;
        let abs_offset = self.byte_offset + byte_offset;
        let bytes = if little_endian {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        self.buffer.with_data_mut(|data| {
            data[abs_offset..abs_offset + 8].copy_from_slice(&bytes);
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::GcRef;
    use crate::memory::MemoryManager;

    fn make_mm() -> Arc<MemoryManager> {
        Arc::new(MemoryManager::new(1024 * 1024))
    }

    #[test]
    fn test_create_dataview() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(16, None, mm));
        let dv = JsDataView::new(buf.clone(), 0, None).unwrap();
        assert_eq!(dv.byte_length(), 16);
        assert_eq!(dv.byte_offset(), 0);
    }

    #[test]
    fn test_create_with_offset() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(16, None, mm));
        let dv = JsDataView::new(buf.clone(), 4, Some(8)).unwrap();
        assert_eq!(dv.byte_length(), 8);
        assert_eq!(dv.byte_offset(), 4);
    }

    #[test]
    fn test_int8() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(4, None, mm));
        let dv = JsDataView::new(buf, 0, None).unwrap();

        dv.set_int8(0, -128).unwrap();
        dv.set_int8(1, 127).unwrap();
        dv.set_int8(2, -1).unwrap();

        assert_eq!(dv.get_int8(0).unwrap(), -128);
        assert_eq!(dv.get_int8(1).unwrap(), 127);
        assert_eq!(dv.get_int8(2).unwrap(), -1);
    }

    #[test]
    fn test_uint8() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(4, None, mm));
        let dv = JsDataView::new(buf, 0, None).unwrap();

        dv.set_uint8(0, 0).unwrap();
        dv.set_uint8(1, 255).unwrap();
        dv.set_uint8(2, 128).unwrap();

        assert_eq!(dv.get_uint8(0).unwrap(), 0);
        assert_eq!(dv.get_uint8(1).unwrap(), 255);
        assert_eq!(dv.get_uint8(2).unwrap(), 128);
    }

    #[test]
    fn test_int16_endianness() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(4, None, mm));
        let dv = JsDataView::new(buf, 0, None).unwrap();

        dv.set_int16(0, 0x0102, true).unwrap(); // Little-endian: 02 01
        assert_eq!(dv.get_int16(0, true).unwrap(), 0x0102);
        assert_eq!(dv.get_int16(0, false).unwrap(), 0x0201); // Big-endian read

        dv.set_int16(2, 0x0304, false).unwrap(); // Big-endian: 03 04
        assert_eq!(dv.get_int16(2, false).unwrap(), 0x0304);
        assert_eq!(dv.get_int16(2, true).unwrap(), 0x0403); // Little-endian read
    }

    #[test]
    fn test_int32_endianness() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(8, None, mm));
        let dv = JsDataView::new(buf, 0, None).unwrap();

        dv.set_int32(0, 0x01020304, true).unwrap();
        assert_eq!(dv.get_int32(0, true).unwrap(), 0x01020304);

        dv.set_int32(4, 0x05060708, false).unwrap();
        assert_eq!(dv.get_int32(4, false).unwrap(), 0x05060708);
    }

    #[test]
    fn test_float32() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(8, None, mm));
        let dv = JsDataView::new(buf, 0, None).unwrap();

        dv.set_float32(0, 3.14, true).unwrap();
        let val = dv.get_float32(0, true).unwrap();
        assert!((val - 3.14).abs() < 0.001);

        dv.set_float32(4, 2.71, false).unwrap();
        let val = dv.get_float32(4, false).unwrap();
        assert!((val - 2.71).abs() < 0.001);
    }

    #[test]
    fn test_float64() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(16, None, mm));
        let dv = JsDataView::new(buf, 0, None).unwrap();

        dv.set_float64(0, std::f64::consts::PI, true).unwrap();
        let val = dv.get_float64(0, true).unwrap();
        assert!((val - std::f64::consts::PI).abs() < 1e-10);

        dv.set_float64(8, std::f64::consts::E, false).unwrap();
        let val = dv.get_float64(8, false).unwrap();
        assert!((val - std::f64::consts::E).abs() < 1e-10);
    }

    #[test]
    fn test_big_int64() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(16, None, mm));
        let dv = JsDataView::new(buf, 0, None).unwrap();

        dv.set_big_int64(0, i64::MAX, true).unwrap();
        assert_eq!(dv.get_big_int64(0, true).unwrap(), i64::MAX);

        dv.set_big_int64(8, i64::MIN, false).unwrap();
        assert_eq!(dv.get_big_int64(8, false).unwrap(), i64::MIN);
    }

    #[test]
    fn test_bounds_check() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(4, None, mm));
        let dv = JsDataView::new(buf, 0, None).unwrap();

        assert!(dv.get_int32(1, true).is_err()); // Would read past end
        assert!(dv.get_int8(4).is_err()); // Out of bounds
    }

    #[test]
    fn test_detached_buffer() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(16, None, mm));
        let dv = JsDataView::new(buf.clone(), 0, None).unwrap();

        dv.set_int32(0, 42, true).unwrap();
        assert_eq!(dv.get_int32(0, true).unwrap(), 42);

        buf.detach();

        assert!(dv.is_detached());
        assert_eq!(dv.byte_length(), 0);
        assert!(dv.get_int32(0, true).is_err());
    }

    #[test]
    fn test_invalid_construction() {
        let mm = make_mm();
        let buf = GcRef::new(JsArrayBuffer::new(8, None, mm));

        // Offset past end
        assert!(JsDataView::new(buf.clone(), 10, None).is_err());

        // Length extends past end
        assert!(JsDataView::new(buf.clone(), 4, Some(10)).is_err());

        // Detached buffer
        buf.detach();
        assert!(JsDataView::new(buf, 0, None).is_err());
    }
}
