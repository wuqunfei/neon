//
// Restore chunks from local Zenith repository
//
// This runs once at Page Server startup. It loads all the "snapshots" and all
// WAL from all timelines from the local zenith repository into the in-memory page
// cache.
//
// This also initializes the "last valid LSN" in the page cache to the last LSN
// seen in the WAL, so that when the WAL receiver is started, it starts
// streaming from that LSN.
//

use log::*;
use regex::Regex;
use std::fmt;

use std::error::Error;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::cmp::max;

use anyhow::Result;
use bytes::Bytes;

use crate::page_cache;
use crate::page_cache::PageCache;
use crate:: PageServerConf;
use crate::page_cache::BufferTag;
use crate::waldecoder::WalStreamDecoder;
use crate::ZTimelineId;


// From pg_tablespace_d.h
//
// FIXME: we'll probably need these elsewhere too, move to some common location
const DEFAULTTABLESPACE_OID: u32 = 1663;
const GLOBALTABLESPACE_OID: u32 = 1664;

//
// Load it all into the page cache.
//
pub fn restore_timeline(conf: &PageServerConf, pcache: &PageCache, timeline: ZTimelineId) -> Result<()> {

    let timelinepath = PathBuf::from("timelines").join(&timeline.to_str());

    if !timelinepath.exists() {
        anyhow::bail!("timeline {} does not exist in the page server's repository");
    }

    // Scan .zenith/timelines/<timeline>/snapshots
    let snapshotspath = "timelines/".to_owned() + &timeline.to_str() + "/snapshots";

    let mut last_snapshot_lsn: u64 = 0;

    for direntry in fs::read_dir(&snapshotspath).unwrap() {
        let filename = direntry.unwrap().file_name().to_str().unwrap().to_owned();

        let lsn = u64::from_str_radix(&filename, 16)?;
        last_snapshot_lsn = max(lsn, last_snapshot_lsn);

        restore_snapshot(conf, pcache, timeline, &filename)?;
        info!("restored snapshot at {}", filename);
    }

    if last_snapshot_lsn == 0 {
        error!("could not find valid snapshot in {}", &snapshotspath);
        // TODO return error?
    }
    pcache.init_valid_lsn(last_snapshot_lsn);

    restore_wal(conf, pcache, timeline, last_snapshot_lsn)?;

    Ok(())
}

pub fn find_latest_snapshot(_conf: &PageServerConf, timeline: ZTimelineId) -> Result<u64> {

    let snapshotspath = format!("timelines/{}/snapshots", timeline);

    let mut last_snapshot_lsn = 0;
    for direntry in fs::read_dir(&snapshotspath).unwrap() {
        let filename = direntry.unwrap().file_name().to_str().unwrap().to_owned();

        let lsn = u64::from_str_radix(&filename, 16)?;
        last_snapshot_lsn = max(lsn, last_snapshot_lsn);
    }

    if last_snapshot_lsn == 0 {
        error!("could not find valid snapshot in {}", &snapshotspath);
        // TODO return error?
    }
    Ok(last_snapshot_lsn)
}

fn restore_snapshot(conf: &PageServerConf, pcache: &PageCache, timeline: ZTimelineId, snapshot: &str) -> Result<()> {

    let snapshotpath = "timelines/".to_owned() + &timeline.to_str() + "/snapshots/" + snapshot;

    // Scan 'global'
    let paths = fs::read_dir(snapshotpath.clone() + "/global").unwrap();

    for direntry in paths {
        let path = direntry.unwrap().path();
        let filename = path.file_name();
        if filename.is_none() {
            continue;
        }
        let filename = filename.unwrap().to_str();

        if filename == Some("pg_control") {
            continue;
        }
        if filename == Some("pg_filenode.map") {
            continue;
        }

        restore_relfile(conf, pcache, timeline, snapshot, GLOBALTABLESPACE_OID, 0, &path)?;
    }

    // Scan 'base'
    let paths = fs::read_dir(snapshotpath.clone() + "/base").unwrap();
    for path in paths {
        let path = path.unwrap();
        let filename = path.file_name().to_str().unwrap().to_owned();

        // Scan database dirs
        let dboid = u32::from_str_radix(&filename, 10)?;

        let paths = fs::read_dir(path.path()).unwrap();
        for direntry in paths {
            let path = direntry.unwrap().path();
            let filename = path.file_name();
            if filename.is_none() {
                continue;
            }
            let filename = filename.unwrap().to_str();
            if filename == Some("PG_VERSION") {
                continue;
            }
            if filename == Some("pg_filenode.map") {
                continue;
            }

            restore_relfile(conf, pcache, timeline, snapshot, DEFAULTTABLESPACE_OID, dboid, &path)?;
        }
    }

    // TODO: Scan pg_tblspc

    Ok(())
}

fn restore_relfile(_conf: &PageServerConf, pcache: &PageCache, _timeline: ZTimelineId, snapshot: &str, spcoid: u32, dboid: u32, path: &Path) -> Result<()> {

    let lsn = u64::from_str_radix(snapshot, 16)?;

    // Does it look like a relation file?

    let p = parse_relfilename(path.file_name().unwrap().to_str().unwrap());
    if p.is_err() {
        let e = p.unwrap_err();
        warn!("unrecognized file in snapshot: {:?} ({})", path, e);
        return Err(e)?;
    }
    let (relnode, forknum, segno) = p.unwrap();

    let mut file = File::open(path)?;
    let mut buf: [u8; 8192] = [0u8; 8192];

    // FIXME: use constants (BLCKSZ)
    let mut blknum: u32 = segno * (1024 * 1024 * 1024 / 8192);
    loop {
        let r = file.read_exact(&mut buf);
        match r {
            Ok(_) => {
                let tag = page_cache::BufferTag {
                    spcnode: spcoid,
                    dbnode: dboid,
                    relnode: relnode,
                    forknum: forknum as u8,
                    blknum: blknum,
                };
                pcache.put_page_image(tag, lsn, Bytes::copy_from_slice(&buf));
                /*
                if oldest_lsn == 0 || p.lsn < oldest_lsn {
                    oldest_lsn = p.lsn;
                }
                 */
            }

            // TODO: UnexpectedEof is expected
            Err(e) => match e.kind() {
                std::io::ErrorKind::UnexpectedEof => {
                    // reached EOF. That's expected.
                    // FIXME: maybe check that we read the full length of the file?
                    break;
                },
                _ => {
                    error!("error reading file: {:?} ({})", path, e);
                    break;
                }
            }
        };
        blknum += 1;
    }

    let tag = page_cache::RelTag {
        spcnode: spcoid,
        dbnode: dboid,
        relnode: relnode,
        forknum: forknum as u8,
    };
    pcache.relsize_inc(&tag, Some(blknum));

    Ok(())
}

// Scan WAL on a timeline, starting from gien LSN, and load all the records
// into the page cache.
fn restore_wal(_conf: &PageServerConf, pcache: &PageCache, timeline: ZTimelineId, startpoint: u64) -> Result<()> {
    let walpath = format!("timelines/{}/wal", timeline);

    let mut waldecoder = WalStreamDecoder::new(u64::from(startpoint));

    let mut segno = XLByteToSeg(startpoint, 16 * 1024 * 1024);
    let mut offset = XLogSegmentOffset(startpoint, 16 * 1024 * 1024);
    let mut last_lsn = 0;
    loop {
        // FIXME: assume postgresql tli 1 for now
        let filename = XLogFileName(1, segno, 16 * 1024 * 1024);
        let mut path = walpath.clone() + "/" + &filename;

        // It could be as .partial
        if !PathBuf::from(&path).exists() {
            path = path + ".partial";
        }

        // Slurp the WAL file
        let open_result = File::open(&path);
        if let Err(e) = open_result {
            if e.kind() == std::io::ErrorKind::NotFound {
                break;
            }
            return Err(e)?;
        }
        let mut file = open_result.unwrap();

        if offset > 0 {
            file.seek(SeekFrom::Start(offset as u64))?;
        }

        let mut buf = Vec::new();
        let nread = file.read_to_end(&mut buf)?;
        if nread != 16 * 1024 * 1024 - offset as usize {
            // Maybe allow this for .partial files?
            error!("read only {} bytes from WAL file", nread);
        }
        waldecoder.feed_bytes(&buf);

        let mut nrecords = 0;
        loop {
            let rec = waldecoder.poll_decode();
            if rec.is_err() {
                // Assume that an error means we've reached the end of
                // a partial WAL record. So that's ok.
                break;
            }
            if let Some((lsn, recdata)) = rec.unwrap() {
                let decoded =
                    crate::waldecoder::decode_wal_record(recdata.clone());

                // Put the WAL record to the page cache. We make a separate copy of
                // it for every block it modifies. (The actual WAL record is kept in
                // a Bytes, which uses a reference counter for the underlying buffer,
                // so having multiple copies of it doesn't cost that much)
                for blk in decoded.blocks.iter() {
                    let tag = BufferTag {
                        spcnode: blk.rnode_spcnode,
                        dbnode: blk.rnode_dbnode,
                        relnode: blk.rnode_relnode,
                        forknum: blk.forknum as u8,
                        blknum: blk.blkno,
                    };

                    let rec = page_cache::WALRecord {
                        lsn: lsn,
                        will_init: blk.will_init || blk.apply_image,
                        rec: recdata.clone(),
                    };

                    pcache.put_wal_record(tag, rec);
                }

                // Now that this record has been handled, let the page cache know that
                // it is up-to-date to this LSN
                pcache.advance_last_valid_lsn(lsn);
                last_lsn = lsn;
            } else {
                break;
            }
            nrecords += 1;
        }

        info!("restored {} records from WAL file {}", nrecords, filename);

        segno += 1;
        offset = 0;
    }
    info!("reached end of WAL at {:X}/{:X}", last_lsn >> 32, last_lsn & 0xffffffff);

    Ok(())
}

// FIXME: copied from xlog_utils.rs
pub const XLOG_FNAME_LEN: usize = 24;
pub type XLogRecPtr = u64;
pub type XLogSegNo = u64;
pub type TimeLineID = u32;

#[allow(non_snake_case)]
pub fn XLogSegmentOffset(xlogptr: XLogRecPtr, wal_segsz_bytes: usize) -> u32 {
    return (xlogptr as u32) & (wal_segsz_bytes as u32 - 1);
}

#[allow(non_snake_case)]
pub fn XLByteToSeg(xlogptr: XLogRecPtr, wal_segsz_bytes: usize) -> XLogSegNo {
    return xlogptr / wal_segsz_bytes as u64;
}


#[allow(non_snake_case)]
pub fn XLogFileName(tli: TimeLineID, logSegNo: XLogSegNo, wal_segsz_bytes: usize) -> String {
    return format!(
        "{:>08X}{:>08X}{:>08X}",
        tli,
        logSegNo / XLogSegmentsPerXLogId(wal_segsz_bytes),
        logSegNo % XLogSegmentsPerXLogId(wal_segsz_bytes)
    );
}

#[allow(non_snake_case)]
pub fn XLogSegmentsPerXLogId(wal_segsz_bytes: usize) -> XLogSegNo {
    return (0x100000000u64 / wal_segsz_bytes as u64) as XLogSegNo;
}

#[allow(non_snake_case)]
pub fn XLogFromFileName(fname: &str, wal_seg_size: usize) -> (XLogSegNo, TimeLineID) {
    let tli = u32::from_str_radix(&fname[0..8], 16).unwrap();
    let log = u32::from_str_radix(&fname[8..16], 16).unwrap() as XLogSegNo;
    let seg = u32::from_str_radix(&fname[16..24], 16).unwrap() as XLogSegNo;
    return (log * XLogSegmentsPerXLogId(wal_seg_size) + seg, tli);
}

#[allow(non_snake_case)]
pub fn IsXLogFileName(fname: &str) -> bool {
    return fname.len() == XLOG_FNAME_LEN && fname.chars().all(|c| c.is_ascii_hexdigit());
}

#[allow(non_snake_case)]
pub fn IsPartialXLogFileName(fname: &str) -> bool {
    if let Some(basefname) = fname.strip_suffix(".partial") {
        IsXLogFileName(basefname)
    } else {
        false
    }
}


#[derive(Debug, Clone)]
struct FilePathError {
    msg: String,
}

impl Error for FilePathError {
    fn description(&self) -> &str {
        &self.msg
    }
}
impl FilePathError {
    fn new(msg: &str) -> FilePathError {
        FilePathError {
            msg: msg.to_string(),
        }
    }
}

impl From<core::num::ParseIntError> for FilePathError {
    fn from(e: core::num::ParseIntError) -> Self {
        return FilePathError {
            msg: format!("invalid filename: {}", e),
        };
    }
}

impl fmt::Display for FilePathError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "invalid filename")
    }
}

fn forkname_to_forknum(forkname: Option<&str>) -> Result<u32, FilePathError> {
    match forkname {
        // "main" is not in filenames, it's implicit if the fork name is not present
        None => Ok(0),
        Some("fsm") => Ok(1),
        Some("vm") => Ok(2),
        Some("init") => Ok(3),
        Some(_) => Err(FilePathError::new("invalid forkname")),
    }
}

#[derive(Debug)]
struct ParsedBaseImageFileName {
    pub spcnode: u32,
    pub dbnode: u32,
    pub relnode: u32,
    pub forknum: u32,
    pub segno: u32,

    pub lsn: u64,
}

// formats:
// <oid>
// <oid>_<fork name>
// <oid>.<segment number>
// <oid>_<fork name>.<segment number>

fn parse_relfilename(fname: &str) -> Result<(u32, u32, u32), FilePathError> {
    let re = Regex::new(r"^(?P<relnode>\d+)(_(?P<forkname>[a-z]+))?(\.(?P<segno>\d+))?$").unwrap();

    let caps = re
        .captures(fname)
        .ok_or_else(|| FilePathError::new("invalid relation data file name"))?;

    let relnode_str = caps.name("relnode").unwrap().as_str();
    let relnode = u32::from_str_radix(relnode_str, 10)?;

    let forkname_match = caps.name("forkname");
    let forkname = if forkname_match.is_none() {
        None
    } else {
        Some(forkname_match.unwrap().as_str())
    };
    let forknum = forkname_to_forknum(forkname)?;

    let segno_match = caps.name("segno");
    let segno = if segno_match.is_none() {
        0
    } else {
        u32::from_str_radix(segno_match.unwrap().as_str(), 10)?
    };

    return Ok((relnode, forknum, segno));
}
