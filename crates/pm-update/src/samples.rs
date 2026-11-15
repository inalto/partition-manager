//! Sample GitHub "latest release" API responses shared by the tests of this crate.

pub(crate) const SAMPLE: &str = r#"{
  "tag_name": "v0.2.3",
  "name": "Partition Manager 0.2.3",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/v0.2.3",
  "draft": false,
  "prerelease": false,
  "assets": [
    {"name": "partition-manager-0.2.3-windows-x86_64.zip",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-windows-x86_64.zip",
     "size": 11111},
    {"name": "partition-manager-0.2.3-linux-x86_64.tar.gz",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-linux-x86_64.tar.gz",
     "size": 22222},
    {"name": "partition-manager-0.2.3-macos-aarch64.tar.gz",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-macos-aarch64.tar.gz",
     "size": 33333},
    {"name": "partition-manager-0.2.3-macos-x86_64.tar.gz",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-macos-x86_64.tar.gz",
     "size": 44444},
    {"name": "SHA256SUMS.txt",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/SHA256SUMS.txt",
     "size": 400}
  ]
}"#;

pub(crate) const SAMPLE_NO_SUMS: &str = r#"{
  "tag_name": "v0.2.3",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/v0.2.3",
  "draft": false,
  "prerelease": false,
  "assets": [
    {"name": "partition-manager-0.2.3-linux-x86_64.tar.gz",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-linux-x86_64.tar.gz",
     "size": 22222}
  ]
}"#;

pub(crate) const SAMPLE_DRAFT: &str = r#"{
  "tag_name": "v0.2.4",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/v0.2.4",
  "draft": true,
  "prerelease": false,
  "assets": []
}"#;

pub(crate) const SAMPLE_PRERELEASE: &str = r#"{
  "tag_name": "v0.3.0",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/v0.3.0",
  "draft": false,
  "prerelease": true,
  "assets": []
}"#;

pub(crate) const SAMPLE_TAG_NIGHTLY: &str = r#"{
  "tag_name": "nightly",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/nightly",
  "draft": false,
  "prerelease": false,
  "assets": []
}"#;
