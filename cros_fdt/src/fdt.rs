// Copyright 2018 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module writes Flattened Devicetree blobs as defined here:
//! <https://devicetree-specification.readthedocs.io/en/stable/flattened-format.html>

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io;

use remain::sorted;
use thiserror::Error as ThisError;

use crate::propval::ToFdtPropval;

pub(crate) const SIZE_U32: usize = std::mem::size_of::<u32>();
pub(crate) const SIZE_U64: usize = std::mem::size_of::<u64>();

#[sorted]
#[derive(ThisError, Debug)]
pub enum Error {
    #[error("I/O error dumping FDT to file code={} path={}", .0, .1.display())]
    FdtDumpIoError(io::Error, std::path::PathBuf),
    #[error("Parse error reading FDT parameters")]
    FdtFileParseError,
    #[error("Error writing FDT to guest memory")]
    FdtGuestMemoryWriteError,
    #[error("I/O error code={0}")]
    FdtIoError(io::Error),
    #[error("Invalid name string: {}", .0)]
    InvalidName(String),
    #[error("Invalid string value {}", .0)]
    InvalidString(String),
    #[error("Property value size must fit in 32 bits")]
    PropertyValueTooLarge,
    #[error("Total size must fit in 32 bits")]
    TotalSizeTooLarge,
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::FdtIoError(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

const FDT_BEGIN_NODE: u32 = 0x00000001;
const FDT_END_NODE: u32 = 0x00000002;
const FDT_PROP: u32 = 0x00000003;
const FDT_END: u32 = 0x00000009;

// Return the number of padding bytes required to align `size` to `alignment`.
#[inline]
fn align_pad_len(size: usize, alignment: usize) -> usize {
    (alignment - size % alignment) % alignment
}

// Pad a byte vector to given alignment.
#[inline]
fn align_data(data: &mut Vec<u8>, alignment: usize) {
    data.resize(align_pad_len(data.len(), alignment) + data.len(), 0u8);
}

// Verify FDT property name.
fn is_valid_prop_name(name: &str) -> bool {
    const ALLOWED_SPECIAL_CHARS: [u8; 7] = [b'.', b',', b'_', b'+', b'?', b'#', b'-'];
    name.bytes()
        .all(|c| c.is_ascii_alphanumeric() || ALLOWED_SPECIAL_CHARS.contains(&c))
}

// Verify FDT node name.
fn is_valid_node_name(name: &str) -> bool {
    const ALLOWED_SPECIAL_CHARS: [u8; 6] = [b'.', b',', b'_', b'+', b'-', b'@'];
    const ADDR_SEP: u8 = b'@';
    // At most one `@` separating node-name and unit-address
    if name.bytes().filter(|&c| c == ADDR_SEP).count() > 1 {
        return false;
    }
    name.bytes()
        .all(|c| c.is_ascii_alphanumeric() || ALLOWED_SPECIAL_CHARS.contains(&c))
}

// An implementation of FDT header.
#[derive(Default)]
struct FdtHeader {
    magic: u32,             // magic word
    total_size: u32,        // total size of DT block
    off_dt_struct: u32,     // offset to structure
    off_dt_strings: u32,    // offset to strings
    off_mem_rsvmap: u32,    // offset to memory reserve map
    version: u32,           // format version
    last_comp_version: u32, // last compatible version
    boot_cpuid_phys: u32,   // Which physical CPU id we're booting on
    size_dt_strings: u32,   // size of the strings block
    size_dt_struct: u32,    // size of the structure block
}

impl FdtHeader {
    const MAGIC: u32 = 0xd00dfeed;
    const VERSION: u32 = 17;
    const LAST_COMP_VERSION: u32 = 16;
    const SIZE: usize = 10 * SIZE_U32;

    // Create a new FdtHeader instance.
    fn new(
        total_size: u32,
        off_dt_struct: u32,
        off_dt_strings: u32,
        off_mem_rsvmap: u32,
        boot_cpuid_phys: u32,
        size_dt_strings: u32,
        size_dt_struct: u32,
    ) -> Self {
        Self {
            magic: Self::MAGIC,
            total_size,
            off_dt_struct,
            off_dt_strings,
            off_mem_rsvmap,
            version: Self::VERSION,
            last_comp_version: Self::LAST_COMP_VERSION,
            boot_cpuid_phys,
            size_dt_strings,
            size_dt_struct,
        }
    }

    // Dump FDT header to a byte vector.
    fn write_blob(&self, buffer: &mut [u8]) -> Result<()> {
        assert_eq!(buffer.len(), Self::SIZE);
        for (chunk, val_u32) in buffer.chunks_exact_mut(SIZE_U32).zip(&[
            self.magic,
            self.total_size,
            self.off_dt_struct,
            self.off_dt_strings,
            self.off_mem_rsvmap,
            self.version,
            self.last_comp_version,
            self.boot_cpuid_phys,
            self.size_dt_strings,
            self.size_dt_struct,
        ]) {
            chunk.copy_from_slice(&val_u32.to_be_bytes());
        }
        Ok(())
    }
}

// An implementation of FDT strings block (property names)
#[derive(Default)]
struct FdtStrings {
    strings: Vec<u8>,
    string_offsets: BTreeMap<CString, u32>,
}

impl FdtStrings {
    // Find an existing instance of a string `s`, or add it to the strings block.
    // Returns the offset into the strings block.
    fn intern_string(&mut self, s: CString) -> u32 {
        if let Some(off) = self.string_offsets.get(&s) {
            *off
        } else {
            let off = self.strings.len() as u32;
            self.strings.extend_from_slice(s.to_bytes_with_nul());
            self.string_offsets.insert(s, off);
            off
        }
    }

    // Write the strings blob to a `Write` object.
    fn write_blob(&self, mut writer: impl io::Write) -> Result<()> {
        Ok(writer.write_all(&self.strings)?)
    }
}

/// Flattened device tree node.
///
/// This represents a single node from the FDT structure block. Every node may contain properties
/// and other (child) nodes.
#[derive(Debug, Clone)]
pub struct FdtNode {
    /// Node name
    pub(crate) name: String,
    pub(crate) props: BTreeMap<String, Vec<u8>>,
    pub(crate) subnodes: BTreeMap<String, FdtNode>,
}

impl FdtNode {
    // Create a new node with the given name, properties, and child nodes. Return an error if
    // node or property names do not satisfy devicetree naming criteria.
    pub(crate) fn new(
        name: String,
        props: BTreeMap<String, Vec<u8>>,
        subnodes: BTreeMap<String, FdtNode>,
    ) -> Result<Self> {
        if !is_valid_node_name(&name) {
            return Err(Error::InvalidName(name));
        }
        for pname in props.keys() {
            if !is_valid_prop_name(pname) {
                return Err(Error::InvalidName(pname.into()));
            }
        }
        Ok(Self {
            name,
            props,
            subnodes,
        })
    }

    // Create an empty node with the given name.
    pub(crate) fn empty(name: impl Into<String>) -> Result<Self> {
        FdtNode::new(name.into(), [].into(), [].into())
    }

    // Write binary contents of a node to a vector of bytes.
    fn write_blob(&self, writer: &mut impl io::Write, strings: &mut FdtStrings) -> Result<()> {
        // Token
        writer.write_all(&FDT_BEGIN_NODE.to_be_bytes())?;
        // Name
        writer.write_all(self.name.as_bytes())?;
        writer.write_all(&[0])?; // Node name terminator
        let pad_len = align_pad_len(self.name.len() + 1, SIZE_U32);
        writer.write_all(&vec![0; pad_len])?;
        // Properties
        for (propname, propblob) in self.props.iter() {
            // Prop token
            writer.write_all(&FDT_PROP.to_be_bytes())?;
            // Prop size
            writer.write_all(&(propblob.len() as u32).to_be_bytes())?;
            // Prop name offset
            let propname = CString::new(propname.as_str()).expect("\\0 in property name");
            writer.write_all(&strings.intern_string(propname).to_be_bytes())?;
            // Prop value
            writer.write_all(propblob)?;
            let pad_len = align_pad_len(propblob.len(), SIZE_U32);
            writer.write_all(&vec![0; pad_len])?;
        }
        // Subnodes
        for subnode in self.subnodes.values() {
            subnode.write_blob(writer, strings)?;
        }
        // Token
        writer.write_all(&FDT_END_NODE.to_be_bytes())?;
        Ok(())
    }

    /// Write a property.
    ///
    /// # Arguments
    ///
    /// `name` - name of the property; must be a valid property name according to DT spec.
    /// `val` - value of the property (raw byte array).
    pub fn set_prop<T>(&mut self, name: &str, value: T) -> Result<()>
    where
        T: ToFdtPropval,
    {
        if !is_valid_prop_name(name) {
            return Err(Error::InvalidName(name.into()));
        }
        let bytes = value.to_propval()?;
        // FDT property byte size must fit into a u32.
        u32::try_from(bytes.len()).map_err(|_| Error::PropertyValueTooLarge)?;
        self.props.insert(name.into(), bytes);
        Ok(())
    }

    /// Create a node if it doesn't already exist, and return a mutable reference to it. Return
    /// an error if the node name is not valid.
    ///
    /// # Arguments
    ///
    /// `name` - name of the node; must be a valid node name according to DT specification.
    pub fn subnode_mut(&mut self, name: &str) -> Result<&mut FdtNode> {
        if !self.subnodes.contains_key(name) {
            self.subnodes.insert(name.into(), FdtNode::empty(name)?);
        }
        Ok(self.subnodes.get_mut(name).unwrap())
    }
}

/// Interface for creating and manipulating a Flattened Devicetree (FDT) and emitting
/// a Devicetree Blob (DTB).
///
/// # Example
///
/// ```rust
/// use cros_fdt::Fdt;
///
/// # fn main() -> cros_fdt::Result<()> {
/// let mut fdt = Fdt::new(&[]);
/// let root_node = fdt.root_mut();
/// root_node.set_prop("compatible", "linux,dummy-virt")?;
/// root_node.set_prop("#address-cells", 0x2u32)?;
/// root_node.set_prop("#size-cells", 0x2u32)?;
/// let chosen_node = root_node.subnode_mut("chosen")?;
/// chosen_node.set_prop("linux,pci-probe-only", 1u32)?;
/// chosen_node.set_prop("bootargs", "panic=-1 console=hvc0 root=/dev/vda")?;
/// let dtb = fdt.finish().unwrap();
/// # Ok(())
/// # }
/// ```
pub struct Fdt {
    pub(crate) reserved_memory: Vec<FdtReserveEntry>,
    pub(crate) root: FdtNode,
    strings: FdtStrings,
    boot_cpuid_phys: u32,
}

/// Reserved physical memory region.
///
/// This represents an area of physical memory reserved by the firmware and unusable by the OS.
/// For example, this could be used to preserve bootloader code or data used at runtime.
#[derive(Clone)]
pub struct FdtReserveEntry {
    /// Physical address of the beginning of the reserved region.
    pub address: u64,
    /// Size of the reserved region in bytes.
    pub size: u64,
}

// Last entry in the reserved memory section
const RESVMEM_TERMINATOR: FdtReserveEntry = FdtReserveEntry::new(0, 0);

impl FdtReserveEntry {
    /// Create a new FdtReserveEntry
    ///
    /// # Arguments
    ///
    /// `address` - start of reserved memory region.
    /// `size` - size of reserved memory region.
    pub const fn new(address: u64, size: u64) -> Self {
        Self { address, size }
    }

    // Dump the entry as a vector of bytes.
    fn write_blob(&self, mut writer: impl io::Write) -> Result<()> {
        writer.write_all(&self.address.to_be_bytes())?;
        writer.write_all(&self.size.to_be_bytes())?;
        Ok(())
    }
}

impl Fdt {
    /// Create a new flattened device tree instance with an initialized root node.
    ///
    /// # Arguments
    ///
    /// `mem_reservations` - reserved physical memory regions to list in the FDT header.
    pub fn new(mem_reservations: &[FdtReserveEntry]) -> Self {
        Self {
            reserved_memory: mem_reservations.to_vec(),
            root: FdtNode::empty("").unwrap(),
            strings: FdtStrings::default(),
            boot_cpuid_phys: 0u32,
        }
    }

    /// Set the `boot_cpuid_phys` field of the devicetree header.
    ///
    /// # Arguments
    ///
    /// `boot_cpuid_phys` - CPU ID
    pub fn set_boot_cpuid_phys(&mut self, boot_cpuid_phys: u32) {
        self.boot_cpuid_phys = boot_cpuid_phys;
    }

    // Write the reserved memory block to a buffer.
    fn write_reserved_memory(&self, mut writer: impl io::Write) -> Result<()> {
        for entry in &self.reserved_memory {
            entry.write_blob(&mut writer)?;
        }
        RESVMEM_TERMINATOR.write_blob(writer)
    }

    // Write the structure block of the FDT.
    fn write_struct(&mut self, mut writer: impl io::Write) -> Result<()> {
        self.root.write_blob(&mut writer, &mut self.strings)?;
        writer.write_all(&FDT_END.to_be_bytes())?;
        Ok(())
    }

    /// Finish writing the Devicetree Blob (DTB).
    ///
    /// Returns the DTB as a vector of bytes.
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        let mut result = vec![0u8; FdtHeader::SIZE];
        align_data(&mut result, SIZE_U64);

        let off_mem_rsvmap = result.len();
        self.write_reserved_memory(&mut result)?;
        align_data(&mut result, SIZE_U64);

        let off_dt_struct = result.len();
        self.write_struct(&mut result)?;
        align_data(&mut result, SIZE_U32);

        let off_dt_strings = result.len();
        self.strings.write_blob(&mut result)?;
        let total_size = u32::try_from(result.len()).map_err(|_| Error::TotalSizeTooLarge)?;

        let header = FdtHeader::new(
            total_size,
            off_dt_struct as u32,
            off_dt_strings as u32,
            off_mem_rsvmap as u32,
            self.boot_cpuid_phys,
            total_size - off_dt_strings as u32, // strings size
            off_dt_strings as u32 - off_dt_struct as u32, // struct size
        );
        header.write_blob(&mut result[..FdtHeader::SIZE])?;
        Ok(result)
    }

    /// Return a mutable reference to the root node of the FDT.
    pub fn root_mut(&mut self) -> &mut FdtNode {
        &mut self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal() {
        let mut fdt = Fdt::new(&[]);
        assert_eq!(
            fdt.finish().unwrap(),
            [
                0xd0, 0x0d, 0xfe, 0xed, // 0000: magic (0xd00dfeed)
                0x00, 0x00, 0x00, 0x48, // 0004: totalsize (0x48)
                0x00, 0x00, 0x00, 0x38, // 0008: off_dt_struct (0x38)
                0x00, 0x00, 0x00, 0x48, // 000C: off_dt_strings (0x48)
                0x00, 0x00, 0x00, 0x28, // 0010: off_mem_rsvmap (0x28)
                0x00, 0x00, 0x00, 0x11, // 0014: version (0x11 = 17)
                0x00, 0x00, 0x00, 0x10, // 0018: last_comp_version (0x10 = 16)
                0x00, 0x00, 0x00, 0x00, // 001C: boot_cpuid_phys (0)
                0x00, 0x00, 0x00, 0x00, // 0020: size_dt_strings (0)
                0x00, 0x00, 0x00, 0x10, // 0024: size_dt_struct (0x10)
                0x00, 0x00, 0x00, 0x00, // 0028: rsvmap terminator (address = 0 high)
                0x00, 0x00, 0x00, 0x00, // 002C: rsvmap terminator (address = 0 low)
                0x00, 0x00, 0x00, 0x00, // 0030: rsvmap terminator (size = 0 high)
                0x00, 0x00, 0x00, 0x00, // 0034: rsvmap terminator (size = 0 low)
                0x00, 0x00, 0x00, 0x01, // 0038: FDT_BEGIN_NODE
                0x00, 0x00, 0x00, 0x00, // 003C: node name ("") + padding
                0x00, 0x00, 0x00, 0x02, // 0040: FDT_END_NODE
                0x00, 0x00, 0x00, 0x09, // 0044: FDT_END
            ]
        );
    }

    #[test]
    fn reservemap() {
        let mut fdt = Fdt::new(&[
            FdtReserveEntry {
                address: 0x12345678AABBCCDD,
                size: 0x1234,
            },
            FdtReserveEntry {
                address: 0x1020304050607080,
                size: 0x5678,
            },
        ]);
        assert_eq!(
            fdt.finish().unwrap(),
            [
                0xd0, 0x0d, 0xfe, 0xed, // 0000: magic (0xd00dfeed)
                0x00, 0x00, 0x00, 0x68, // 0004: totalsize (0x68)
                0x00, 0x00, 0x00, 0x58, // 0008: off_dt_struct (0x58)
                0x00, 0x00, 0x00, 0x68, // 000C: off_dt_strings (0x68)
                0x00, 0x00, 0x00, 0x28, // 0010: off_mem_rsvmap (0x28)
                0x00, 0x00, 0x00, 0x11, // 0014: version (0x11 = 17)
                0x00, 0x00, 0x00, 0x10, // 0018: last_comp_version (0x10 = 16)
                0x00, 0x00, 0x00, 0x00, // 001C: boot_cpuid_phys (0)
                0x00, 0x00, 0x00, 0x00, // 0020: size_dt_strings (0)
                0x00, 0x00, 0x00, 0x10, // 0024: size_dt_struct (0x10)
                0x12, 0x34, 0x56, 0x78, // 0028: rsvmap entry 0 address high
                0xAA, 0xBB, 0xCC, 0xDD, // 002C: rsvmap entry 0 address low
                0x00, 0x00, 0x00, 0x00, // 0030: rsvmap entry 0 size high
                0x00, 0x00, 0x12, 0x34, // 0034: rsvmap entry 0 size low
                0x10, 0x20, 0x30, 0x40, // 0038: rsvmap entry 1 address high
                0x50, 0x60, 0x70, 0x80, // 003C: rsvmap entry 1 address low
                0x00, 0x00, 0x00, 0x00, // 0040: rsvmap entry 1 size high
                0x00, 0x00, 0x56, 0x78, // 0044: rsvmap entry 1 size low
                0x00, 0x00, 0x00, 0x00, // 0048: rsvmap terminator (address = 0 high)
                0x00, 0x00, 0x00, 0x00, // 004C: rsvmap terminator (address = 0 low)
                0x00, 0x00, 0x00, 0x00, // 0050: rsvmap terminator (size = 0 high)
                0x00, 0x00, 0x00, 0x00, // 0054: rsvmap terminator (size = 0 low)
                0x00, 0x00, 0x00, 0x01, // 0058: FDT_BEGIN_NODE
                0x00, 0x00, 0x00, 0x00, // 005C: node name ("") + padding
                0x00, 0x00, 0x00, 0x02, // 0060: FDT_END_NODE
                0x00, 0x00, 0x00, 0x09, // 0064: FDT_END
            ]
        );
    }

    #[test]
    fn prop_null() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        root_node.set_prop("null", ()).unwrap();
        assert_eq!(
            fdt.finish().unwrap(),
            [
                0xd0, 0x0d, 0xfe, 0xed, // 0000: magic (0xd00dfeed)
                0x00, 0x00, 0x00, 0x59, // 0004: totalsize (0x59)
                0x00, 0x00, 0x00, 0x38, // 0008: off_dt_struct (0x38)
                0x00, 0x00, 0x00, 0x54, // 000C: off_dt_strings (0x54)
                0x00, 0x00, 0x00, 0x28, // 0010: off_mem_rsvmap (0x28)
                0x00, 0x00, 0x00, 0x11, // 0014: version (0x11 = 17)
                0x00, 0x00, 0x00, 0x10, // 0018: last_comp_version (0x10 = 16)
                0x00, 0x00, 0x00, 0x00, // 001C: boot_cpuid_phys (0)
                0x00, 0x00, 0x00, 0x05, // 0020: size_dt_strings (0x05)
                0x00, 0x00, 0x00, 0x1c, // 0024: size_dt_struct (0x1C)
                0x00, 0x00, 0x00, 0x00, // 0028: rsvmap terminator (address = 0 high)
                0x00, 0x00, 0x00, 0x00, // 002C: rsvmap terminator (address = 0 low)
                0x00, 0x00, 0x00, 0x00, // 0030: rsvmap terminator (size = 0 high)
                0x00, 0x00, 0x00, 0x00, // 0034: rsvmap terminator (size = 0 low)
                0x00, 0x00, 0x00, 0x01, // 0038: FDT_BEGIN_NODE
                0x00, 0x00, 0x00, 0x00, // 003C: node name ("") + padding
                0x00, 0x00, 0x00, 0x03, // 0040: FDT_PROP
                0x00, 0x00, 0x00, 0x00, // 0044: prop len (0)
                0x00, 0x00, 0x00, 0x00, // 0048: prop nameoff (0)
                0x00, 0x00, 0x00, 0x02, // 004C: FDT_END_NODE
                0x00, 0x00, 0x00, 0x09, // 0050: FDT_END
                b'n', b'u', b'l', b'l', 0x00, // 0054: strings block
            ]
        );
    }

    #[test]
    fn prop_u32() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        root_node.set_prop("u32", 0x12345678u32).unwrap();
        assert_eq!(
            fdt.finish().unwrap(),
            [
                0xd0, 0x0d, 0xfe, 0xed, // 0000: magic (0xd00dfeed)
                0x00, 0x00, 0x00, 0x5c, // 0004: totalsize (0x5C)
                0x00, 0x00, 0x00, 0x38, // 0008: off_dt_struct (0x38)
                0x00, 0x00, 0x00, 0x58, // 000C: off_dt_strings (0x58)
                0x00, 0x00, 0x00, 0x28, // 0010: off_mem_rsvmap (0x28)
                0x00, 0x00, 0x00, 0x11, // 0014: version (0x11 = 17)
                0x00, 0x00, 0x00, 0x10, // 0018: last_comp_version (0x10 = 16)
                0x00, 0x00, 0x00, 0x00, // 001C: boot_cpuid_phys (0)
                0x00, 0x00, 0x00, 0x04, // 0020: size_dt_strings (0x04)
                0x00, 0x00, 0x00, 0x20, // 0024: size_dt_struct (0x20)
                0x00, 0x00, 0x00, 0x00, // 0028: rsvmap terminator (address = 0 high)
                0x00, 0x00, 0x00, 0x00, // 002C: rsvmap terminator (address = 0 low)
                0x00, 0x00, 0x00, 0x00, // 0030: rsvmap terminator (size = 0 high)
                0x00, 0x00, 0x00, 0x00, // 0034: rsvmap terminator (size = 0 low)
                0x00, 0x00, 0x00, 0x01, // 0038: FDT_BEGIN_NODE
                0x00, 0x00, 0x00, 0x00, // 003C: node name ("") + padding
                0x00, 0x00, 0x00, 0x03, // 0040: FDT_PROP
                0x00, 0x00, 0x00, 0x04, // 0044: prop len (4)
                0x00, 0x00, 0x00, 0x00, // 0048: prop nameoff (0)
                0x12, 0x34, 0x56, 0x78, // 004C: prop u32 value (0x12345678)
                0x00, 0x00, 0x00, 0x02, // 0050: FDT_END_NODE
                0x00, 0x00, 0x00, 0x09, // 0054: FDT_END
                b'u', b'3', b'2', 0x00, // 0058: strings block
            ]
        );
    }

    #[test]
    fn all_props() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        root_node
            .set_prop("arru32", &[0x12345678u32, 0xAABBCCDDu32])
            .unwrap();
        root_node
            .set_prop("arru64", &[0x1234567887654321u64])
            .unwrap();
        root_node.set_prop("null", ()).unwrap();
        root_node.set_prop("str", "hello").unwrap();
        root_node.set_prop("strlst", &["hi", "bye"]).unwrap();
        root_node.set_prop("u32", 0x12345678u32).unwrap();
        root_node.set_prop("u64", 0x1234567887654321u64).unwrap();
        assert_eq!(
            fdt.finish().unwrap(),
            [
                0xd0, 0x0d, 0xfe, 0xed, // 0000: magic (0xd00dfeed)
                0x00, 0x00, 0x00, 0xee, // 0004: totalsize (0xEE)
                0x00, 0x00, 0x00, 0x38, // 0008: off_dt_struct (0x38)
                0x00, 0x00, 0x00, 0xc8, // 000C: off_dt_strings (0xC8)
                0x00, 0x00, 0x00, 0x28, // 0010: off_mem_rsvmap (0x28)
                0x00, 0x00, 0x00, 0x11, // 0014: version (0x11 = 17)
                0x00, 0x00, 0x00, 0x10, // 0018: last_comp_version (0x10 = 16)
                0x00, 0x00, 0x00, 0x00, // 001C: boot_cpuid_phys (0)
                0x00, 0x00, 0x00, 0x26, // 0020: size_dt_strings (0x26)
                0x00, 0x00, 0x00, 0x90, // 0024: size_dt_struct (0x90)
                0x00, 0x00, 0x00, 0x00, // 0028: rsvmap terminator (address = 0 high)
                0x00, 0x00, 0x00, 0x00, // 002C: rsvmap terminator (address = 0 low)
                0x00, 0x00, 0x00, 0x00, // 0030: rsvmap terminator (size = 0 high)
                0x00, 0x00, 0x00, 0x00, // 0034: rsvmap terminator (size = 0 low)
                0x00, 0x00, 0x00, 0x01, // 0038: FDT_BEGIN_NODE
                0x00, 0x00, 0x00, 0x00, // 003C: node name ("") + padding
                0x00, 0x00, 0x00, 0x03, // 0040: FDT_PROP (u32 array)
                0x00, 0x00, 0x00, 0x08, // 0044: prop len (8)
                0x00, 0x00, 0x00, 0x00, // 0048: prop nameoff (0x00)
                0x12, 0x34, 0x56, 0x78, // 004C: prop value 0
                0xAA, 0xBB, 0xCC, 0xDD, // 0050: prop value 1
                0x00, 0x00, 0x00, 0x03, // 0054: FDT_PROP (u64 array)
                0x00, 0x00, 0x00, 0x08, // 0058: prop len (8)
                0x00, 0x00, 0x00, 0x07, // 005C: prop nameoff (0x07)
                0x12, 0x34, 0x56, 0x78, // 0060: prop u64 value 0 high
                0x87, 0x65, 0x43, 0x21, // 0064: prop u64 value 0 low
                0x00, 0x00, 0x00, 0x03, // 0068: FDT_PROP (null)
                0x00, 0x00, 0x00, 0x00, // 006C: prop len (0)
                0x00, 0x00, 0x00, 0x0E, // 0070: prop nameoff (0x0e)
                0x00, 0x00, 0x00, 0x03, // 0074: FDT_PROP (string)
                0x00, 0x00, 0x00, 0x06, // 0078: prop len (6)
                0x00, 0x00, 0x00, 0x13, // 007C: prop nameoff (0x13)
                b'h', b'e', b'l', b'l', // 0080: prop str value ("hello") + padding
                b'o', 0x00, 0x00, 0x00, // 0084: "o\0" + padding
                0x00, 0x00, 0x00, 0x03, // 0088: FDT_PROP (string list)
                0x00, 0x00, 0x00, 0x07, // 008C: prop len (7)
                0x00, 0x00, 0x00, 0x17, // 0090: prop nameoff (0x17)
                b'h', b'i', 0x00, b'b', // 0094: prop value ("hi", "bye")
                b'y', b'e', 0x00, 0x00, // 0098: "ye\0" + padding
                0x00, 0x00, 0x00, 0x03, // 009C: FDT_PROP (u32)
                0x00, 0x00, 0x00, 0x04, // 00A0: prop len (4)
                0x00, 0x00, 0x00, 0x1E, // 00A4: prop nameoff (0x1E)
                0x12, 0x34, 0x56, 0x78, // 00A8: prop u32 value (0x12345678)
                0x00, 0x00, 0x00, 0x03, // 00AC: FDT_PROP (u64)
                0x00, 0x00, 0x00, 0x08, // 00B0: prop len (8)
                0x00, 0x00, 0x00, 0x22, // 00B4: prop nameoff (0x22)
                0x12, 0x34, 0x56, 0x78, // 00B8: prop u64 value high (0x12345678)
                0x87, 0x65, 0x43, 0x21, // 00BC: prop u64 value low (0x87654321)
                0x00, 0x00, 0x00, 0x02, // 00C0: FDT_END_NODE
                0x00, 0x00, 0x00, 0x09, // 00C4: FDT_END
                b'a', b'r', b'r', b'u', b'3', b'2', 0x00, // 00C8: strings + 0x00: "arru32"
                b'a', b'r', b'r', b'u', b'6', b'4', 0x00, // 00CF: strings + 0x07: "arru64"
                b'n', b'u', b'l', b'l', 0x00, // 00D6: strings + 0x0E: "null"
                b's', b't', b'r', 0x00, // 00DB: strings + 0x13: "str"
                b's', b't', b'r', b'l', b's', b't', 0x00, // 00DF: strings + 0x17: "strlst"
                b'u', b'3', b'2', 0x00, // 00E6: strings + 0x1E: "u32"
                b'u', b'6', b'4', 0x00, // 00EA: strings + 0x22: "u64"
            ]
        );
    }

    #[test]
    fn nested_nodes() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        root_node.set_prop("abc", 0x13579024u32).unwrap();
        let nested_node = root_node.subnode_mut("nested").unwrap();
        nested_node.set_prop("def", 0x12121212u32).unwrap();
        assert_eq!(
            fdt.finish().unwrap(),
            [
                0xd0, 0x0d, 0xfe, 0xed, // 0000: magic (0xd00dfeed)
                0x00, 0x00, 0x00, 0x80, // 0004: totalsize (0x80)
                0x00, 0x00, 0x00, 0x38, // 0008: off_dt_struct (0x38)
                0x00, 0x00, 0x00, 0x78, // 000C: off_dt_strings (0x78)
                0x00, 0x00, 0x00, 0x28, // 0010: off_mem_rsvmap (0x28)
                0x00, 0x00, 0x00, 0x11, // 0014: version (0x11 = 17)
                0x00, 0x00, 0x00, 0x10, // 0018: last_comp_version (0x10 = 16)
                0x00, 0x00, 0x00, 0x00, // 001C: boot_cpuid_phys (0)
                0x00, 0x00, 0x00, 0x08, // 0020: size_dt_strings (0x08)
                0x00, 0x00, 0x00, 0x40, // 0024: size_dt_struct (0x40)
                0x00, 0x00, 0x00, 0x00, // 0028: rsvmap terminator (address = 0 high)
                0x00, 0x00, 0x00, 0x00, // 002C: rsvmap terminator (address = 0 low)
                0x00, 0x00, 0x00, 0x00, // 0030: rsvmap terminator (size = 0 high)
                0x00, 0x00, 0x00, 0x00, // 0034: rsvmap terminator (size = 0 low)
                0x00, 0x00, 0x00, 0x01, // 0038: FDT_BEGIN_NODE
                0x00, 0x00, 0x00, 0x00, // 003C: node name ("") + padding
                0x00, 0x00, 0x00, 0x03, // 0040: FDT_PROP
                0x00, 0x00, 0x00, 0x04, // 0044: prop len (4)
                0x00, 0x00, 0x00, 0x00, // 0048: prop nameoff (0x00)
                0x13, 0x57, 0x90, 0x24, // 004C: prop u32 value (0x13579024)
                0x00, 0x00, 0x00, 0x01, // 0050: FDT_BEGIN_NODE
                b'n', b'e', b's', b't', // 0054: Node name ("nested")
                b'e', b'd', 0x00, 0x00, // 0058: "ed\0" + pad
                0x00, 0x00, 0x00, 0x03, // 005C: FDT_PROP
                0x00, 0x00, 0x00, 0x04, // 0060: prop len (4)
                0x00, 0x00, 0x00, 0x04, // 0064: prop nameoff (0x04)
                0x12, 0x12, 0x12, 0x12, // 0068: prop u32 value (0x12121212)
                0x00, 0x00, 0x00, 0x02, // 006C: FDT_END_NODE ("nested")
                0x00, 0x00, 0x00, 0x02, // 0070: FDT_END_NODE ("")
                0x00, 0x00, 0x00, 0x09, // 0074: FDT_END
                b'a', b'b', b'c', 0x00, // 0078: strings + 0x00: "abc"
                b'd', b'e', b'f', 0x00, // 007C: strings + 0x04: "def"
            ]
        );
    }

    #[test]
    fn prop_name_string_reuse() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        root_node.set_prop("abc", 0x13579024u32).unwrap();
        let nested = root_node.subnode_mut("nested").unwrap();
        nested.set_prop("abc", 0x12121212u32).unwrap(); // This should reuse the "abc" string.
        nested.set_prop("def", 0x12121212u32).unwrap();
        assert_eq!(
            fdt.finish().unwrap(),
            [
                0xd0, 0x0d, 0xfe, 0xed, // 0000: magic (0xd00dfeed)
                0x00, 0x00, 0x00, 0x90, // 0004: totalsize (0x90)
                0x00, 0x00, 0x00, 0x38, // 0008: off_dt_struct (0x38)
                0x00, 0x00, 0x00, 0x88, // 000C: off_dt_strings (0x88)
                0x00, 0x00, 0x00, 0x28, // 0010: off_mem_rsvmap (0x28)
                0x00, 0x00, 0x00, 0x11, // 0014: version (0x11 = 17)
                0x00, 0x00, 0x00, 0x10, // 0018: last_comp_version (0x10 = 16)
                0x00, 0x00, 0x00, 0x00, // 001C: boot_cpuid_phys (0)
                0x00, 0x00, 0x00, 0x08, // 0020: size_dt_strings (0x08)
                0x00, 0x00, 0x00, 0x50, // 0024: size_dt_struct (0x50)
                0x00, 0x00, 0x00, 0x00, // 0028: rsvmap terminator (address = 0 high)
                0x00, 0x00, 0x00, 0x00, // 002C: rsvmap terminator (address = 0 low)
                0x00, 0x00, 0x00, 0x00, // 0030: rsvmap terminator (size = 0 high)
                0x00, 0x00, 0x00, 0x00, // 0034: rsvmap terminator (size = 0 low)
                0x00, 0x00, 0x00, 0x01, // 0038: FDT_BEGIN_NODE
                0x00, 0x00, 0x00, 0x00, // 003C: node name ("") + padding
                0x00, 0x00, 0x00, 0x03, // 0040: FDT_PROP
                0x00, 0x00, 0x00, 0x04, // 0044: prop len (4)
                0x00, 0x00, 0x00, 0x00, // 0048: prop nameoff (0x00)
                0x13, 0x57, 0x90, 0x24, // 004C: prop u32 value (0x13579024)
                0x00, 0x00, 0x00, 0x01, // 0050: FDT_BEGIN_NODE
                b'n', b'e', b's', b't', // 0054: Node name ("nested")
                b'e', b'd', 0x00, 0x00, // 0058: "ed\0" + pad
                0x00, 0x00, 0x00, 0x03, // 005C: FDT_PROP
                0x00, 0x00, 0x00, 0x04, // 0060: prop len (4)
                0x00, 0x00, 0x00, 0x00, // 0064: prop nameoff (0x00 - reuse)
                0x12, 0x12, 0x12, 0x12, // 0068: prop u32 value (0x12121212)
                0x00, 0x00, 0x00, 0x03, // 006C: FDT_PROP
                0x00, 0x00, 0x00, 0x04, // 0070: prop len (4)
                0x00, 0x00, 0x00, 0x04, // 0074: prop nameoff (0x04)
                0x12, 0x12, 0x12, 0x12, // 0078: prop u32 value (0x12121212)
                0x00, 0x00, 0x00, 0x02, // 007C: FDT_END_NODE ("nested")
                0x00, 0x00, 0x00, 0x02, // 0080: FDT_END_NODE ("")
                0x00, 0x00, 0x00, 0x09, // 0084: FDT_END
                b'a', b'b', b'c', 0x00, // 0088: strings + 0x00: "abc"
                b'd', b'e', b'f', 0x00, // 008C: strings + 0x04: "def"
            ]
        );
    }

    #[test]
    fn invalid_node_name_nul() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        root_node
            .subnode_mut("abc\0def")
            .expect_err("node name with embedded NUL");
    }

    #[test]
    fn invalid_prop_name_nul() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        root_node
            .set_prop("abc\0def", 0u32)
            .expect_err("property name with embedded NUL");
    }

    #[test]
    fn invalid_prop_string_value_nul() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        root_node
            .set_prop("mystr", "abc\0def")
            .expect_err("string property value with embedded NUL");
    }

    #[test]
    fn invalid_prop_string_list_value_nul() {
        let mut fdt = Fdt::new(&[]);
        let root_node = fdt.root_mut();
        let strs = ["test", "abc\0def"];
        root_node
            .set_prop("mystr", &strs)
            .expect_err("stringlist property value with embedded NUL");
    }
}
