// Diagnostic reproductions against public APIs; all mutations use newly created temporary fixtures.
use barqsync::{cli::Options, entry::DirEnt, fsops, output::{Output, StatsSnapshot}, proto::{Conn, Frame, Msg, RootInfo}, remote::{receiver, sender}};
use std::{fs, path::Path, os::unix::net::UnixStream, time::Duration};
fn pair() -> (Conn, Conn) {
    let (a,b)=UnixStream::pair().unwrap();
    let mut x=Conn::start(a.try_clone().unwrap(),a,false); x.seal();
    let mut y=Conn::start(b.try_clone().unwrap(),b,false); y.seal(); (x,y)
}
fn next(c:&Conn)->Frame { c.rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap() }
fn receive_case(base:&Path, kind:&str) {
    let dir=base.join(kind); fs::create_dir(&dir).unwrap();
    let dest=dir.join("dest"); fs::create_dir(&dest).unwrap();
    let seed=dir.join("seed"); fs::write(&seed,b"GOOD").unwrap();
    let meta=fsops::lstat(&seed).unwrap(); let dm=fsops::stat(&dest).unwrap();
    let name=if kind=="traversal" { "../escaped" } else { "file" };
    let opts=Options::parse_from(["barqsync","-a","--threads=2","s/","d/"]).unwrap();
    let (actual,peer)=pair();
    let handle=std::thread::spawn(move|| { let out=Output::new(&opts); receiver::run(&actual,&opts,dest.as_os_str(),&out).0 });
    peer.msg(Msg::Roots(vec![RootInfo { contents:true, meta:Some(dm), items:vec![], sel:None, extra:None }])).unwrap();
    peer.msg(Msg::DirBatch {root:0,dir:0,rel:vec![],entries:vec![DirEnt::new(name.into(),meta,None)],sums:vec![]}).unwrap();
    loop { if matches!(next(&peer),Frame::Msg(Msg::Need {..})) {break;} }
    if kind=="short_pack" {
        peer.msg(Msg::SmallPack {files:vec![(0,4)],blob:vec![b'X'],xattrs:vec![]}).unwrap();
    } else {
        peer.msg(Msg::FileBegin {id:0,size:4,xattrs:None}).unwrap();
        if kind=="traversal" {peer.send(Frame::Data {id:0,off:0,bytes:b"GOOD".to_vec()}).unwrap();}
        peer.msg(Msg::FileEnd {id:0,ok:true}).unwrap();
    }
    peer.msg(Msg::ScanDone {dirs:1}).unwrap();
    peer.msg(Msg::SenderDone {errors:0}).unwrap();
    loop {if matches!(next(&peer),Frame::Msg(Msg::ReceiverDone {..})) {break;}}
    let code=handle.join().unwrap();
    let path=if kind=="traversal" {dir.join("escaped")} else {dir.join("dest/file")};
    println!("{kind}: exit={code}, result={:?}",fs::read(path));
}
fn delete_case(base:&Path,dry:bool) {
    let dir=base.join(if dry {"delete_dry"} else {"delete_without_flag"});fs::create_dir(&dir).unwrap();
    let src=dir.join("src");fs::create_dir(&src).unwrap();fs::write(src.join("kept"),b"kept").unwrap();
    let victim=dir.join("outside-source");fs::write(&victim,b"SAVE").unwrap();let meta=fsops::lstat(&victim).unwrap();
    let mut opts=Options::parse_from(["barqsync","-a","--threads=2","s/","d/"]).unwrap();opts.dry_run=dry;
    let (actual,peer)=pair(); let sources=vec![format!("{}/",src.display()).into()];
    let handle=std::thread::spawn(move|| {let out=Output::new(&opts);sender::run(&actual,&opts,&sources,&out).0});
    loop { if let Frame::Msg(Msg::DirBatch {dir,..})=next(&peer) {peer.msg(Msg::Need{dir,files:vec![],xattrs:vec![]}).unwrap();break;} }
    use std::os::unix::ffi::OsStrExt;
    peer.msg(Msg::Synced {root:0,files:vec![(victim.as_os_str().as_bytes().to_vec(),meta.size,meta.mtime)]}).unwrap();
    peer.msg(Msg::ReceiverDone{stats:StatsSnapshot::default()}).unwrap();
    let code=handle.join().unwrap();println!("delete dry={dry} remove_source_files=false: exit={code}, outside_source_exists={}",victim.exists());
}
fn main() {
    let dir=std::env::temp_dir().join(format!("barqsync-probes-{}",std::process::id()));fs::create_dir(&dir).unwrap();
    for k in ["traversal","missing_data","short_pack"] {receive_case(&dir,k);}
    delete_case(&dir,false);delete_case(&dir,true);
    let p=dir.join("same-second-change");fs::write(&p,b"OLD!").unwrap();fsops::set_mtime(&p,1600000000,100).unwrap();
    let m=fsops::lstat(&p).unwrap();fs::write(&p,b"NEW!").unwrap();fsops::set_mtime(&p,1600000000,900000000).unwrap();
    let opts=Options::parse_from(["barqsync","-a","--remove-source-files","s/","d/"]).unwrap();
    let r=barqsync::local::flags::remove_source(&opts,&p,m.size,m.mtime);
    println!("same_second_source_change: result={r:?}, exists={}",p.exists());
    println!("fixtures={}",dir.display());
}
