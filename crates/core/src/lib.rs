//! Shared logic for Card Intake, used by the desktop app and the NAS service.

pub mod cameras;
pub mod card_details;
pub mod deliver;
pub mod destinations;
pub mod eject;
pub mod exif;
pub mod exiftool;
pub mod grouping;
pub mod health;
pub mod import;
pub mod ledger;
pub mod naming;
pub mod nas;
pub mod process;
pub mod rating;
pub mod scan;
pub mod shutter;
pub mod storage;
pub mod volume_info;
pub mod volumes;
mod xmp;
