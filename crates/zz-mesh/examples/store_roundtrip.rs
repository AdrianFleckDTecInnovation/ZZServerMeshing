//! Kept compilable with the original crate for the two-direction format test.
use zz_mesh::*;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or_else(|| invalid("expected read or write"))?;
    let path = args
        .next()
        .ok_or_else(|| invalid("missing database path"))?;
    let db = Store::open(path)?;
    let group = Id::numbered(11);
    let id = group.child(47);
    if mode == "write" {
        let w = db.start_worker(3)?;
        let a = db.create_group(group, w)?;
        let mut r = Record::new(id, group);
        r.put(4097, 3, &(id, 0x12_3456u64))?;
        db.checkpoint(group, a, 0, vec![r])?;
    } else if mode != "read" {
        return Err(invalid("expected read or write"));
    }
    let s = db.snapshot(group)?;
    assert_eq!((s.group, s.revision, s.records.len()), (group, 1, 1));
    let r = &s.records[0];
    assert_eq!(
        (r.id, r.group, r.get::<(Id, u64)>(4097, 3)?),
        (id, group, (id, 0x12_3456))
    );
    let req = encode(&wire::Request::Load {
        group: Some(group),
        required: vec![],
    })?;
    let reply = encode(&wire::Response::Stopped)?;
    println!(
        "{id} {:?} v{} {req:02x?} {reply:02x?}",
        s.hash()?,
        wire::WIRE_VERSION
    );
    Ok(())
}
