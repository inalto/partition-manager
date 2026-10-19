//! In-memory [`Platform`] for tests: disks are [`MemDevice`]s, side effects are recorded.

use crate::merge_volumes;
use crate::traits::{fs_format_name, ExternalTool, MountPoint, OpenMode, Platform, SmartData, WriteSession};
use pm_core::{
    BlockDevice, DiskInfo, Error, FsKind, MemDevice, ProgressReport, ProgressSink, ReadOnly,
    Result, SliceDevice, VolumeId, VolumeInfo,
};
use pm_parttable::{read_table, write_table, Table};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

type SharedDisk = Arc<Mutex<MemDevice>>;

struct MockDisk {
    dev: SharedDisk,
    meta: DiskInfo,
}

#[derive(Default)]
struct State {
    disks: BTreeMap<u32, MockDisk>,
    volumes: Vec<VolumeInfo>,
    mounted: Vec<(VolumeId, MountPoint)>,
    sessions: Vec<u32>,
    /// Disk indexes of sessions whose `finish` was called (shared with the sessions).
    finished: Arc<Mutex<Vec<u32>>>,
    calls: Vec<String>,
    fail_prefixes: Vec<String>,
    system_disk: Option<u32>,
    smart: BTreeMap<u32, SmartData>,
}

/// Test double for [`Platform`].
///
/// * disks are `Arc<Mutex<MemDevice>>`; [`MockPlatform::disk_bytes`] exposes their content;
/// * `enumerate_disks` re-reads the partition table on every call;
/// * `format`, `shrink_volume`, `extend_volume`, `set_offline`, `rescan` and `run_tool` are
///   recorded in [`MockPlatform::calls`] and succeed unless [`MockPlatform::fail_calls_matching`]
///   was used.
#[derive(Default)]
pub struct MockPlatform {
    state: Mutex<State>,
}

/// Letters the mock reports as free (minus those currently mounted).
const MOCK_FREE_LETTERS: [char; 3] = ['E', 'F', 'G'];

impl MockPlatform {
    pub fn new() -> Self {
        Self::default()
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add (or replace) disk `index`. `meta` supplies model/serial/bus/flags; size, sector size,
    /// table and partitions are derived from `dev` when enumerating.
    pub fn add_disk(&self, index: u32, dev: MemDevice, meta: DiskInfo) {
        self.st().disks.insert(
            index,
            MockDisk {
                dev: Arc::new(Mutex::new(dev)),
                meta,
            },
        );
    }

    /// Add a volume living on `disk` at byte range `[start, start + len)`.
    pub fn add_volume(&self, mut v: VolumeInfo, disk: u32, start: u64, len: u64) {
        v.disk = Some(disk);
        v.start = start;
        v.len = len;
        let mut st = self.st();
        st.volumes.retain(|x| x.id != v.id);
        st.volumes.push(v);
    }

    /// Add a volume that lives on no single disk (spanned / virtual): its `disk` is `None`.
    pub fn add_spanned_volume(&self, mut v: VolumeInfo) {
        v.disk = None;
        let mut st = self.st();
        st.volumes.retain(|x| x.id != v.id);
        st.volumes.push(v);
    }

    /// Make [`Platform::smart`] return `data` for disk `index` (other disks report `None`).
    pub fn set_smart(&self, index: u32, data: SmartData) {
        self.st().smart.insert(index, data);
    }

    pub fn set_system_disk(&self, index: u32) {
        self.st().system_disk = Some(index);
    }

    /// Current `(volume, mount point)` pairs added through [`Platform::mount`].
    pub fn mounted(&self) -> Vec<(VolumeId, MountPoint)> {
        self.st().mounted.clone()
    }

    /// Disk indexes passed to [`Platform::begin_write`], in order.
    pub fn write_sessions_log(&self) -> Vec<u32> {
        self.st().sessions.clone()
    }

    /// Disk indexes of write sessions on which [`WriteSession::finish`] was called, in order.
    /// A session dropped without `finish` does not appear here.
    pub fn finished_sessions_log(&self) -> Vec<u32> {
        let finished = self.st().finished.clone();
        let log = finished.lock().unwrap_or_else(|e| e.into_inner()).clone();
        log
    }

    /// Recorded side-effect calls, e.g. `"format <id> NTFS label=Data quick=true"`,
    /// `"shrink <id> <new_len>"`, `"run_tool BcdBoot [\"..\"]"`.
    pub fn calls(&self) -> Vec<String> {
        self.st().calls.clone()
    }

    /// Make every later recorded call whose text starts with `prefix` fail with
    /// `Error::Platform` (the call is still recorded).
    pub fn fail_calls_matching(&self, prefix: &str) {
        self.st().fail_prefixes.push(prefix.to_string());
    }

    /// Snapshot of the bytes of disk `index` (panics if the disk does not exist).
    pub fn disk_bytes(&self, index: u32) -> Vec<u8> {
        let dev = self.disk(index).expect("mock disk exists");
        let guard = dev.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_slice().to_vec()
    }

    fn disk(&self, index: u32) -> Result<SharedDisk> {
        self.st()
            .disks
            .get(&index)
            .map(|d| d.dev.clone())
            .ok_or_else(|| Error::NotFound(format!("disk {index}")))
    }

    fn record(&self, call: String) -> Result<()> {
        let mut st = self.st();
        let fail = st.fail_prefixes.iter().any(|p| call.starts_with(p.as_str()));
        st.calls.push(call.clone());
        if fail {
            Err(Error::Platform {
                code: 1,
                msg: format!("mock failure: {call}"),
            })
        } else {
            Ok(())
        }
    }

    fn volume_mut<'a>(st: &'a mut State, id: &VolumeId) -> Result<&'a mut VolumeInfo> {
        st.volumes
            .iter_mut()
            .find(|v| &v.id == id)
            .ok_or_else(|| Error::NotFound(format!("volume {}", id.0)))
    }

    fn device(dev: &SharedDisk, mode: OpenMode) -> Box<dyn BlockDevice> {
        let d = MockDiskDevice(dev.clone());
        match mode {
            OpenMode::ReadOnly => Box::new(ReadOnly(d)),
            OpenMode::ReadWrite => Box::new(d),
        }
    }
}

/// [`BlockDevice`] adapter over a shared mock disk; locks the mutex per call.
struct MockDiskDevice(SharedDisk);

impl MockDiskDevice {
    fn lock(&self) -> MutexGuard<'_, MemDevice> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl BlockDevice for MockDiskDevice {
    fn len(&self) -> u64 {
        self.lock().len()
    }
    fn sector_size(&self) -> u32 {
        self.lock().sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.lock().read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.lock().write_at(offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

struct MockSession {
    disk: u32,
    dev: MockDiskDevice,
    finished: Arc<Mutex<Vec<u32>>>,
}

impl WriteSession for MockSession {
    fn disk(&self) -> u32 {
        self.disk
    }
    fn device(&mut self) -> &mut dyn BlockDevice {
        &mut self.dev
    }
    fn commit_table(&mut self, t: &Table) -> Result<()> {
        write_table(&mut self.dev, t)
    }
    fn finish(self: Box<Self>) -> Result<()> {
        self.finished.lock().unwrap_or_else(|e| e.into_inner()).push(self.disk);
        Ok(())
    }
}

impl Platform for MockPlatform {
    fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        let st = self.st();
        let mut out = Vec::new();
        for (&index, d) in &st.disks {
            let mut dev = d.dev.lock().unwrap_or_else(|e| e.into_inner());
            let base = DiskInfo {
                index,
                path: if d.meta.path.is_empty() {
                    format!("mock:{index}")
                } else {
                    d.meta.path.clone()
                },
                size: dev.len(),
                sector_size: dev.sector_size(),
                physical_sector_size: if d.meta.physical_sector_size == 0 {
                    dev.sector_size()
                } else {
                    d.meta.physical_sector_size
                },
                ..d.meta.clone()
            };
            let mut info = match read_table(&mut *dev) {
                Ok(t) => t.to_disk_info(base),
                // e.g. GPT before the GPT back end exists: keep the caller-supplied
                // table/partitions from `meta`
                Err(e) => {
                    log::debug!("mock disk {index}: read_table failed: {e}");
                    base
                }
            };
            info.flags.system = st.system_disk == Some(index);
            merge_volumes(&mut info, &st.volumes);
            out.push(info);
        }
        Ok(out)
    }

    fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(self.st().volumes.clone())
    }

    fn open_disk(&self, index: u32, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        Ok(Self::device(&self.disk(index)?, mode))
    }

    fn open_volume(&self, id: &VolumeId, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        let (disk, start, len) = {
            let st = self.st();
            let v = st
                .volumes
                .iter()
                .find(|v| &v.id == id)
                .ok_or_else(|| Error::NotFound(format!("volume {}", id.0)))?;
            let disk = v
                .disk
                .ok_or_else(|| Error::NotFound(format!("disk of volume {}", id.0)))?;
            (disk, v.start, v.len)
        };
        let dev = self.disk(disk)?;
        let slice = SliceDevice::new(MockDiskDevice(dev), start, len)?;
        Ok(match mode {
            OpenMode::ReadOnly => Box::new(ReadOnly(slice)),
            OpenMode::ReadWrite => Box::new(slice),
        })
    }

    fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>> {
        let dev = self.disk(disk)?;
        let finished = {
            let mut st = self.st();
            st.sessions.push(disk);
            st.finished.clone()
        };
        Ok(Box::new(MockSession {
            disk,
            dev: MockDiskDevice(dev),
            finished,
        }))
    }

    fn mount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
        let mut st = self.st();
        if st.mounted.iter().any(|(_, m)| m == at) {
            return Err(Error::Busy(format!("{} is already in use", at.to_path_string())));
        }
        Self::volume_mut(&mut st, id)?.mount_points.push(at.to_path_string());
        st.mounted.push((id.clone(), at.clone()));
        Ok(())
    }

    fn unmount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
        let mut st = self.st();
        let pos = st
            .mounted
            .iter()
            .position(|(v, m)| v == id && m == at)
            .ok_or_else(|| {
                Error::NotFound(format!("{} is not mounted at {}", id.0, at.to_path_string()))
            })?;
        st.mounted.remove(pos);
        let path = at.to_path_string();
        Self::volume_mut(&mut st, id)?.mount_points.retain(|m| *m != path);
        Ok(())
    }

    fn set_offline(&self, disk: u32, offline: bool) -> Result<()> {
        self.record(format!("set_offline {disk} {offline}"))
    }

    fn rescan(&self, disk: u32) -> Result<()> {
        self.record(format!("rescan {disk}"))
    }

    fn format(
        &self,
        id: &VolumeId,
        fs: FsKind,
        label: &str,
        cluster: Option<u32>,
        quick: bool,
        progress: &dyn ProgressSink,
    ) -> Result<()> {
        let mut call = format!(
            "format {} {} label={label} quick={quick}",
            id.0,
            fs_format_name(fs).unwrap_or(fs.display_name())
        );
        if let Some(c) = cluster {
            call.push_str(&format!(" cluster={c}"));
        }
        self.record(call)?;
        if let Ok(v) = Self::volume_mut(&mut self.st(), id) {
            v.fs = Some(fs);
            v.label = label.to_string();
        }
        progress.report(&ProgressReport {
            phase: "format".into(),
            done: 100,
            total: 100,
            ..Default::default()
        });
        Ok(())
    }

    fn shrink_volume(&self, id: &VolumeId, new_len: u64) -> Result<()> {
        self.record(format!("shrink {} {new_len}", id.0))?;
        if let Ok(v) = Self::volume_mut(&mut self.st(), id) {
            v.len = new_len;
        }
        Ok(())
    }

    fn extend_volume(&self, id: &VolumeId, new_len: u64) -> Result<()> {
        self.record(format!("extend {} {new_len}", id.0))?;
        if let Ok(v) = Self::volume_mut(&mut self.st(), id) {
            v.len = new_len;
        }
        Ok(())
    }

    fn smart(&self, disk: u32) -> Result<Option<SmartData>> {
        Ok(self.st().smart.get(&disk).cloned())
    }

    fn is_elevated(&self) -> bool {
        true
    }

    fn system_disk(&self) -> Option<u32> {
        self.st().system_disk
    }

    fn free_letters(&self) -> Vec<char> {
        let st = self.st();
        MOCK_FREE_LETTERS
            .iter()
            .copied()
            .filter(|c| !st.mounted.iter().any(|(_, m)| *m == MountPoint::Letter(*c)))
            .collect()
    }

    fn run_tool(&self, tool: ExternalTool, args: &[String]) -> Result<String> {
        self.record(format!("run_tool {tool:?} {args:?}"))?;
        Ok(String::new())
    }
}
