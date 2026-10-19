//! Mount wizard (a drive letter, or an empty folder) → [`Operation::Mount`]; Unmount wizard
//! (pick one of the current mount points) → [`Operation::Unmount`].

use super::{letter_combo, partition_desc, row};
use crate::selection::Selected;
use crate::wizard::{shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::VolumeId;
use pm_ops::Operation;
use pm_platform::MountPoint;
use std::path::{Path, PathBuf};

/// `"E:\"` / `"e:"` → `Letter('E')`, anything else → a folder mount point.
pub fn mount_point_of(m: &str) -> MountPoint {
    let b = m.as_bytes();
    let is_root = b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/')));
    if is_root {
        MountPoint::Letter((b[0] as char).to_ascii_uppercase())
    } else {
        MountPoint::Folder(m.into())
    }
}

/// Why `path` cannot be a mount point (it must be an existing, empty folder given by its full
/// path), if it cannot.
pub fn folder_problem(path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() {
        return Some("Enter the folder to mount the volume in.".into());
    }
    let p = Path::new(path);
    if !p.is_absolute() {
        return Some("Enter the full path of the folder, e.g. C:\\Mount\\Data.".into());
    }
    if !p.is_dir() {
        return Some("The folder does not exist.".into());
    }
    match std::fs::read_dir(p) {
        Err(e) => Some(format!("Cannot read the folder: {e}.")),
        Ok(mut entries) => entries.next().is_some().then(|| "The folder is not empty.".into()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Letter,
    Folder,
}

pub struct MountState {
    pub volume: VolumeId,
    pub desc: String,
    pub target: Target,
    pub letter: Option<char>,
    pub letters: Vec<char>,
    pub folder: String,
    /// `folder` as last checked and the result (the check touches the filesystem, so it only
    /// runs when the text changes, not on every frame).
    checked: Option<(String, Option<String>)>,
}

impl MountState {
    pub fn initial(ctx: &WizardCtx) -> Option<MountState> {
        let Selected::Partition(disk, number) = ctx.selection else { return None };
        let p = ctx.model.partition(disk, number)?;
        Some(MountState {
            volume: p.volume.clone()?,
            desc: partition_desc(disk, p),
            target: Target::Letter,
            letter: ctx.platform_free_letters.first().copied(),
            letters: ctx.platform_free_letters.clone(),
            folder: String::new(),
            checked: None,
        })
    }

    /// [`folder_problem`] of `folder`, remembered until the text changes.
    fn folder_check(&mut self) -> Option<String> {
        let r = self.folder_result();
        self.checked = Some((self.folder.clone(), r.clone()));
        r
    }

    /// The remembered check when it is for the current text, a fresh one otherwise.
    fn folder_result(&self) -> Option<String> {
        match &self.checked {
            Some((f, r)) if *f == self.folder => r.clone(),
            _ => folder_problem(&self.folder),
        }
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        let at = match self.target {
            Target::Letter => MountPoint::Letter(self.letter.ok_or("Choose a drive letter.")?),
            Target::Folder => {
                if let Some(p) = self.folder_result() {
                    return Err(p);
                }
                MountPoint::Folder(PathBuf::from(self.folder.trim()))
            }
        };
        Ok(vec![Operation::Mount { volume: self.volume.clone(), at }])
    }
}

fn mount_ui(s: &mut MountState, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
    ui.label(&s.desc);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.radio_value(&mut s.target, Target::Letter, "Drive letter:");
        ui.add_enabled_ui(s.target == Target::Letter, |ui| letter_combo(ui, "mount_letter", &mut s.letter, &s.letters));
    });
    ui.horizontal(|ui| {
        ui.radio_value(&mut s.target, Target::Folder, "Empty folder:");
        ui.add_enabled(
            s.target == Target::Folder,
            egui::TextEdit::singleline(&mut s.folder).desired_width(260.0).hint_text("C:\\Mount\\Data"),
        );
    });
    ui.weak("A folder mount point must be an empty folder on an NTFS volume.");
    let problem = match s.target {
        Target::Letter => s.letter.is_none().then(|| "Choose a drive letter.".to_string()),
        Target::Folder => s.folder_check(),
    };
    StepResult::from_problem(problem)
}

fn mount_summary(s: &MountState) -> Vec<(String, String)> {
    let at = match s.target {
        Target::Letter => s.letter.map_or("-".to_string(), |c| format!("{c}:\\")),
        Target::Folder => s.folder.trim().to_string(),
    };
    vec![row("Volume", s.desc.clone()), row("Mount at", at)]
}

pub fn wizard(state: MountState) -> Wizard {
    Wizard::over_state("Mount", shared(state), vec![("Mount point", mount_ui, mount_summary)], MountState::build)
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    MountState::initial(ctx).map(wizard)
}

pub struct UnmountState {
    pub volume: VolumeId,
    pub desc: String,
    pub mount_points: Vec<String>,
    pub choice: usize,
}

impl UnmountState {
    pub fn initial(ctx: &WizardCtx) -> Option<UnmountState> {
        let Selected::Partition(disk, number) = ctx.selection else { return None };
        let p = ctx.model.partition(disk, number)?;
        if p.mount_points.is_empty() {
            return None;
        }
        Some(UnmountState {
            volume: p.volume.clone()?,
            desc: partition_desc(disk, p),
            mount_points: p.mount_points.clone(),
            choice: 0,
        })
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        let m = self.mount_points.get(self.choice).ok_or("Choose a mount point.")?;
        Ok(vec![Operation::Unmount { volume: self.volume.clone(), at: mount_point_of(m) }])
    }
}

fn unmount_ui(s: &mut UnmountState, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
    ui.label(&s.desc);
    ui.add_space(6.0);
    ui.label("Remove this mount point:");
    for (i, m) in s.mount_points.iter().enumerate() {
        ui.radio_value(&mut s.choice, i, m);
    }
    ui.weak("The data stays on the volume; only the access path is removed.");
    StepResult::Continue
}

fn unmount_summary(s: &UnmountState) -> Vec<(String, String)> {
    let m = s.mount_points.get(s.choice).cloned().unwrap_or_default();
    vec![row("Volume", s.desc.clone()), row("Remove", m)]
}

pub fn unmount_wizard(state: UnmountState) -> Wizard {
    Wizard::over_state("Unmount", shared(state), vec![("Mount point", unmount_ui, unmount_summary)], UnmountState::build)
}

pub fn new_unmount(ctx: &WizardCtx) -> Option<Wizard> {
    UnmountState::initial(ctx).map(unmount_wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};
    use crate::wizard::check_ops;
    use pm_core::{FsKind, VolumeInfo, MIB};

    fn model() -> pm_ops::Model {
        let mut m = model_of(vec![gpt_disk(1)]);
        m.volumes.push(VolumeInfo {
            id: VolumeId("vol-e".into()),
            disk: Some(1),
            start: 117 * MIB,
            len: 684 * MIB,
            fs: Some(FsKind::Ntfs),
            mount_points: vec!["E:\\".into()],
            ..Default::default()
        });
        m
    }

    #[test]
    fn build_mount_ops() {
        let m = model();
        let mut s = MountState::initial(&WizardCtx::new(&m, vec!['G'], Selected::Partition(1, 3))).unwrap();
        assert_eq!(s.letter, Some('G'));
        let ops = s.build().unwrap();
        assert_eq!(ops, vec![Operation::Mount { volume: VolumeId("vol-e".into()), at: MountPoint::Letter('G') }]);
        assert!(check_ops(&m, &ops).is_ok());
        // folder: must exist and be empty
        let dir = std::env::temp_dir().join(format!("pm-gui-mount-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        s.target = Target::Folder;
        s.folder = dir.display().to_string();
        assert!(s.build().unwrap_err().contains("does not exist"));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            s.build().unwrap(),
            vec![Operation::Mount { volume: VolumeId("vol-e".into()), at: MountPoint::Folder(dir.clone()) }]
        );
        std::fs::write(dir.join("x.txt"), b"x").unwrap();
        assert!(s.build().unwrap_err().contains("not empty"));
        std::fs::remove_dir_all(&dir).unwrap();
        s.folder = "relative\\dir".into();
        assert!(s.build().unwrap_err().contains("full path"));
        // no volume: no mount wizard
        assert!(MountState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 2))).is_none());
    }

    #[test]
    fn build_unmount_ops() {
        let m = model();
        let s = UnmountState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 3))).unwrap();
        let ops = s.build().unwrap();
        assert_eq!(ops, vec![Operation::Unmount { volume: VolumeId("vol-e".into()), at: MountPoint::Letter('E') }]);
        assert!(check_ops(&m, &ops).is_ok());
        assert_eq!(unmount_wizard(s).build_ops().unwrap(), ops);
        assert_eq!(mount_point_of("C:\\mnt\\x\\"), MountPoint::Folder("C:\\mnt\\x\\".into()));
        assert_eq!(mount_point_of("f:"), MountPoint::Letter('F'));
    }
}
