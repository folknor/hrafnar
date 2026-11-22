//! Ephemeral exact-invocation IPC. Durable rows remain the only settings source.
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::PathBuf,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);
impl Default for RunId {
    fn default() -> Self {
        Self(crate::ids::generate_uuid())
    }
}
impl TryFrom<String> for RunId {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        crate::harness::SessionId::parse(&value)?;
        Ok(Self(value))
    }
}
impl From<RunId> for String {
    fn from(id: RunId) -> Self {
        id.0
    }
}
impl RunId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub const PROTOCOL: u32 = 1;
#[derive(Serialize, Deserialize)]
pub struct Handoff {
    pub schema: u32,
    pub requested: RunId,
    pub interrupted: bool,
    pub final_session: Option<String>,
    pub sidecar_written: bool,
}
fn path(data: &crate::data::DataDir, id: &RunId) -> Option<PathBuf> {
    Some(data.inflight()?.join(format!("{}.result", id.as_str())))
}
/// Publication follows all recording/recovery attempts, including failed ones.
/// No prompts or responses belong in this public/private-independent IPC store.
pub fn publish(data: &crate::data::DataDir, handoff: &Handoff) -> std::io::Result<()> {
    let path =
        path(data, &handoff.requested).ok_or_else(|| std::io::Error::other("no data directory"))?;
    std::fs::create_dir_all(path.parent().expect("result parent"))?;
    let bytes = serde_json::to_vec(handoff).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("result.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    // link is atomic and refuses an existing receipt. rename would silently
    // replace another publisher's result on a namespace collision.
    let result = (|| {
        file.write_all(&bytes)?;
        std::fs::hard_link(&tmp, &path)
    })();
    let _ = std::fs::remove_file(tmp);
    // Keep receipts long enough for independent waiters. Only completed receipts
    // age out, after a day; neither live markers nor requests are touched here.
    if let Ok(entries) = std::fs::read_dir(path.parent().expect("result parent")) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "result")
                && p != path
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > std::time::Duration::from_secs(86400))
            {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    result
}
pub fn read(data: &crate::data::DataDir, id: &RunId) -> anyhow::Result<Option<Handoff>> {
    let Some(path) = path(data, id) else {
        anyhow::bail!("no data directory for handoff");
    };
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 16384, "oversized run handoff");
    let handoff: Handoff = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        handoff.schema == PROTOCOL && &handoff.requested == id,
        "unsupported or mismatched run handoff"
    );
    Ok(Some(handoff))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipts_are_exact_bounded_and_never_replaced() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        let data = crate::data::DataDir::at(root.clone());
        let id = RunId::default();
        assert!(read(&data, &id).expect("absent").is_none());
        let mut receipt = Handoff {
            schema: PROTOCOL,
            requested: id.clone(),
            interrupted: true,
            final_session: Some("session".into()),
            sidecar_written: false,
        };
        publish(&data, &receipt).expect("publish");
        receipt.sidecar_written = true;
        assert!(publish(&data, &receipt).is_err());
        let read_back = read(&data, &id).expect("read").expect("receipt");
        assert!(read_back.interrupted && !read_back.sidecar_written);
        let other = RunId::default();
        assert!(read(&data, &other).expect("other run").is_none());
        std::fs::write(
            path(&data, &other).expect("path"),
            serde_json::to_vec(&receipt).expect("encode"),
        )
        .expect("write mismatch");
        assert!(read(&data, &other).is_err());
        std::fs::write(path(&data, &other).expect("path"), vec![b' '; 16385]).expect("oversize");
        assert!(read(&data, &other).is_err());
        assert!(RunId::try_from("../escape".to_owned()).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
