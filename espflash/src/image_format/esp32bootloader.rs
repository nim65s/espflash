use crate::chip::Esp32Params;
use crate::elf::{
    merge_segments, update_checksum, CodeSegment, FirmwareImage, RomSegment, ESP_CHECKSUM_MAGIC,
};
use crate::error::{Error, FlashDetectError};
use crate::flasher::FlashSize;
use crate::image_format::{
    EspCommonHeader, ImageFormat, SegmentHeader, ESP_MAGIC, WP_PIN_DISABLED,
};
use crate::{Chip, PartitionTable};
use bytemuck::bytes_of;
use bytemuck::{Pod, Zeroable};
use sha2::{Digest, Sha256};
use std::{borrow::Cow, io::Write, iter::once};

/// Image format for esp32 family chips using a 2nd stage bootloader
pub struct Esp32BootloaderFormat<'a> {
    params: Esp32Params,
    bootloader: Cow<'a, [u8]>,
    partition_table: PartitionTable,
    flash_segment: RomSegment<'a>,
}

impl<'a> Esp32BootloaderFormat<'a> {
    pub fn new(
        image: &'a FirmwareImage,
        chip: Chip,
        params: Esp32Params,
        partition_table: Option<PartitionTable>,
        bootloader: Option<Vec<u8>>,
    ) -> Result<Self, Error> {
        let partition_table = partition_table.unwrap_or_else(|| params.default_partition_table());
        let bootloader = if let Some(bytes) = bootloader {
            Cow::Owned(bytes)
        } else {
            Cow::Borrowed(params.default_bootloader)
        };

        let mut data = Vec::new();

        let header = EspCommonHeader {
            magic: ESP_MAGIC,
            segment_count: 0,
            flash_mode: image.flash_mode as u8,
            flash_config: encode_flash_size(image.flash_size)? + image.flash_frequency as u8,
            entry: image.entry,
        };
        data.write_all(bytes_of(&header))?;

        let extended_header = ExtendedHeader {
            wp_pin: WP_PIN_DISABLED,
            clk_q_drv: 0,
            d_cs_drv: 0,
            gd_wp_drv: 0,
            chip_id: params.chip_id,
            min_rev: 0,
            padding: [0; 8],
            append_digest: 1,
        };
        data.write_all(bytes_of(&extended_header))?;

        let mut checksum = ESP_CHECKSUM_MAGIC;

        let flash_segments: Vec<_> = merge_segments(image.rom_segments(chip).collect());
        let mut ram_segments: Vec<_> = merge_segments(image.ram_segments(chip).collect());

        let mut segment_count = 0;

        for segment in flash_segments {
            loop {
                let pad_len = get_segment_padding(data.len(), &segment);
                if pad_len > 0 {
                    if pad_len > SEG_HEADER_LEN {
                        if let Some(ram_segment) = ram_segments.first_mut() {
                            // save up to `pad_len` from the ram segment, any remaining bits in the ram segments will be saved later
                            let pad_segment = ram_segment.split_off(pad_len as usize);
                            checksum = save_segment(&mut data, &pad_segment, checksum)?;
                            if ram_segment.data().is_empty() {
                                ram_segments.remove(0);
                            }
                            segment_count += 1;
                            continue;
                        }
                    }
                    let pad_header = SegmentHeader {
                        addr: 0,
                        length: pad_len as u32,
                    };
                    data.write_all(bytes_of(&pad_header))?;
                    for _ in 0..pad_len {
                        data.write_all(&[0])?;
                    }
                    segment_count += 1;
                } else {
                    break;
                }
            }
            checksum = save_flash_segment(&mut data, &segment, checksum)?;
            segment_count += 1;
        }

        for segment in ram_segments {
            checksum = save_segment(&mut data, &segment, checksum)?;
            segment_count += 1;
        }

        let padding = 15 - (data.len() % 16);
        let padding = &[0u8; 16][0..padding as usize];
        data.write_all(padding)?;

        data.write_all(&[checksum])?;

        // since we added some dummy segments, we need to patch the segment count
        data[1] = segment_count as u8;

        let mut hasher = Sha256::new();
        hasher.update(&data);
        let hash = hasher.finalize();
        data.write_all(&hash)?;

        let flash_segment = RomSegment {
            addr: params.app_addr,
            data: Cow::Owned(data),
        };
        Ok(Self {
            params,
            bootloader,
            partition_table,
            flash_segment,
        })
    }
}

impl<'a> ImageFormat<'a> for Esp32BootloaderFormat<'a> {
    fn segments<'b>(&'b self) -> Box<dyn Iterator<Item = RomSegment<'b>> + 'b>
    where
        'a: 'b,
    {
        Box::new(
            once(RomSegment {
                addr: self.params.boot_addr,
                data: Cow::Borrowed(&self.bootloader),
            })
            .chain(once(RomSegment {
                addr: self.params.partition_addr,
                data: self.partition_table.to_bytes().into(),
            }))
            .chain(once(self.flash_segment.borrow())),
        )
    }
}

fn encode_flash_size(size: FlashSize) -> Result<u8, FlashDetectError> {
    match size {
        FlashSize::Flash256Kb => Err(FlashDetectError::from(size as u8)),
        FlashSize::Flash512Kb => Err(FlashDetectError::from(size as u8)),
        FlashSize::Flash1Mb => Ok(0x00),
        FlashSize::Flash2Mb => Ok(0x10),
        FlashSize::Flash4Mb => Ok(0x20),
        FlashSize::Flash8Mb => Ok(0x30),
        FlashSize::Flash16Mb => Ok(0x40),
        FlashSize::FlashRetry => Err(FlashDetectError::from(size as u8)),
    }
}

const IROM_ALIGN: u32 = 65536;
const SEG_HEADER_LEN: u32 = 8;

/// Actual alignment (in data bytes) required for a segment header: positioned
/// so that after we write the next 8 byte header, file_offs % IROM_ALIGN ==
/// segment.addr % IROM_ALIGN
///
/// (this is because the segment's vaddr may not be IROM_ALIGNed, more likely is
/// aligned IROM_ALIGN+0x18 to account for the binary file header
fn get_segment_padding(offset: usize, segment: &CodeSegment) -> u32 {
    let align_past = (segment.addr - SEG_HEADER_LEN) % IROM_ALIGN;
    let pad_len = ((IROM_ALIGN - ((offset as u32) % IROM_ALIGN)) + align_past) % IROM_ALIGN;
    if pad_len == 0 || pad_len == IROM_ALIGN {
        0
    } else if pad_len > SEG_HEADER_LEN {
        pad_len - SEG_HEADER_LEN
    } else {
        pad_len + IROM_ALIGN - SEG_HEADER_LEN
    }
}

fn save_flash_segment(
    data: &mut Vec<u8>,
    segment: &CodeSegment,
    checksum: u8,
) -> Result<u8, Error> {
    let end_pos = (data.len() + segment.data().len()) as u32 + SEG_HEADER_LEN;
    let segment_reminder = end_pos % IROM_ALIGN;

    let checksum = save_segment(data, segment, checksum)?;

    if segment_reminder < 0x24 {
        // Work around a bug in ESP-IDF 2nd stage bootloader, that it didn't map the
        // last MMU page, if an IROM/DROM segment was < 0x24 bytes over the page
        // boundary.
        data.write_all(&[0u8; 0x24][0..(0x24 - segment_reminder as usize)])?;
    }
    Ok(checksum)
}

fn save_segment(data: &mut Vec<u8>, segment: &CodeSegment, checksum: u8) -> Result<u8, Error> {
    let padding = (4 - segment.size() % 4) % 4;

    let header = SegmentHeader {
        addr: segment.addr,
        length: segment.size() + padding,
    };
    data.write_all(bytes_of(&header))?;
    data.write_all(segment.data())?;

    let padding = &[0u8; 4][0..padding as usize];
    data.write_all(padding)?;

    Ok(update_checksum(segment.data(), checksum))
}

#[derive(Copy, Clone, Zeroable, Pod)]
#[repr(C)]
struct ExtendedHeader {
    wp_pin: u8,
    clk_q_drv: u8,
    d_cs_drv: u8,
    gd_wp_drv: u8,
    chip_id: u16,
    min_rev: u8,
    padding: [u8; 8],
    append_digest: u8,
}
