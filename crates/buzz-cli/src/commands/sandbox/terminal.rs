//! Raw TTY forwarding with bounded queues and restoration on every return path.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use buzz_sdk::sandbox_attach::{Flow, Frame, Packet, Rate};
use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use tokio::io::unix::AsyncFd;

use super::*;

struct Tty(AsyncFd<File>);
impl Tty {
    fn open() -> Result<Self, CliError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open("/dev/tty")
            .map_err(failure)?;
        Ok(Self(AsyncFd::new(file).map_err(failure)?))
    }
    async fn read(&self, bytes: &mut [u8]) -> Result<usize, CliError> {
        loop {
            let mut guard = self.0.readable().await.map_err(failure)?;
            match guard.try_io(|fd| {
                let mut file = fd.get_ref();
                file.read(bytes)
            }) {
                Ok(result) => return result.map_err(failure),
                Err(_) => continue,
            }
        }
    }
    async fn write(&self, mut bytes: &[u8]) -> Result<(), CliError> {
        while !bytes.is_empty() {
            let mut guard = self.0.writable().await.map_err(failure)?;
            match guard.try_io(|fd| {
                let mut file = fd.get_ref();
                file.write(bytes)
            }) {
                Ok(Ok(0)) => return Err(failure("terminal output closed")),
                Ok(Ok(n)) => bytes = &bytes[n..],
                Ok(Err(error)) => return Err(failure(error)),
                Err(_) => {}
            }
        }
        Ok(())
    }
}
struct Raw(bool);
impl Raw {
    fn enter() -> Result<Self, CliError> {
        crossterm::terminal::enable_raw_mode().map_err(failure)?;
        Ok(Self(true))
    }
    fn restore(mut self) -> Result<(), CliError> {
        crossterm::terminal::disable_raw_mode().map_err(failure)?;
        self.0 = false;
        Ok(())
    }
}
impl Drop for Raw {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}
fn dimensions() -> Result<(u16, u16), CliError> {
    let (cols, rows) = crossterm::terminal::size().map_err(failure)?;
    Ok((rows.clamp(1, 1000), cols.clamp(1, 1000)))
}
async fn send(
    connection: &mut NostrWsConnection,
    client: &BuzzClient,
    grant: &Grant,
    frame: &Frame,
    rate: &mut Rate,
    clock: Instant,
) -> Result<(), CliError> {
    // Backpressure instead of allocating another queue when the rate is exhausted.
    while !rate.allow(clock.elapsed().as_millis() as u64) {
        sleep(Duration::from_millis(20)).await;
    }
    let event =
        wire::encrypt(client.keys(), grant.scope().agent, grant.scope(), frame).map_err(failure)?;
    timeout(
        Duration::from_secs(1),
        connection.send_raw(&json!(["EVENT", event])),
    )
    .await
    .map_err(|_| failure("terminal frame send timed out"))?
    .map_err(failure)
}

pub(super) async fn attach(
    client: &BuzzClient,
    url: &str,
    owner: PublicKey,
    relay: PublicKey,
    pending: &Pending,
    grant: &Grant,
) -> Result<(), CliError> {
    let tty = Tty::open()?;
    let mut connection = timeout(
        Duration::from_secs(45),
        NostrWsConnection::connect_authenticated(url, client.keys(), None),
    )
    .await
    .map_err(|_| failure("terminal relay connection timed out"))?
    .map_err(failure)?;
    let subscription = json!(["REQ","sandbox-terminal",{"kinds":[24203],"authors":[grant.scope().agent],"#p":[client.keys().public_key()],"#h":[grant.scope().channel],"#e":[grant.scope().thread],"since":now()}]);
    timeout(Duration::from_secs(1), connection.send_raw(&subscription))
        .await
        .map_err(|_| failure("terminal subscription timed out"))?
        .map_err(failure)?;
    authorized(client, owner, relay, pending, grant).await?;
    let mut flow = Flow::new(grant.id());
    let clock = Instant::now();
    let mut outgoing = Rate::default();
    let mut incoming = Rate::default();
    let mut size = dimensions()?;
    let open = flow
        .send(
            Packet::Open {
                rows: size.0,
                cols: size.1,
            },
            0,
        )
        .map_err(failure)?;
    send(&mut connection, client, grant, &open, &mut outgoing, clock).await?;
    let mut raw = None;
    let mut input = [0u8; wire::MAX_DATA_BYTES];
    let mut refresh = Instant::now();
    let mut activity = Instant::now();
    let mut heartbeat = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(failure)?;
    let mut hangup =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).map_err(failure)?;
    let outcome = loop {
        let step:Result<bool,CliError>=async{
            tokio::select!{
                _=terminate.recv()=>return Ok(true),
                _=hangup.recv()=>return Ok(true),
                _=tokio::signal::ctrl_c()=>return Ok(true),
                result=tty.read(&mut input),if raw.is_some()&&flow.writable()=>{
                    let count=result?;
                    let (bytes,detach)=split_input(&input[..count]);
                    if !bytes.is_empty(){
                        let frame=flow.send(Packet::data(bytes).map_err(failure)?,clock.elapsed().as_millis() as u64).map_err(failure)?;
                        send(&mut connection,client,grant,&frame,&mut outgoing,clock).await?;
                        activity=Instant::now();
                    }
                    if count==0||detach{return Ok(true);}
                }
                message=connection.next_event(Duration::from_secs(2))=>{
                    match message{
                        Ok(RelayMessage::Event{subscription_id,event})if subscription_id=="sandbox-terminal"=>{
                            if !incoming.allow(clock.elapsed().as_millis() as u64){return Ok(false);}
                            let frame=wire::decrypt(client.keys(),grant.scope().agent,grant.scope(),grant.id(),&event,now()).map_err(failure)?;
                            let ack=frame.ack;let sequenced=frame.seq!=0;
                            let packet=flow.receive(frame).map_err(failure)?;
                            if raw.is_none()&&ack>=1{
                                eprintln!("Connected. Ctrl-] detaches. Approval expires at {}.",grant.expires_at());
                                raw=Some(Raw::enter()?);
                            }
                            match packet{
                                Some(Packet::Data{data})=>{
                                    let bytes=Packet::Data{data}.bytes().map_err(failure)?;
                                    timeout(Duration::from_secs(2),tty.write(&bytes)).await.map_err(|_|failure("local terminal stopped reading"))??;
                                    if !bytes.is_empty(){activity=Instant::now();}
                                }
                                Some(Packet::Close)=>return Ok(true),
                                Some(Packet::Open{..}|Packet::Resize{..})=>return Err(failure("agent sent an invalid terminal control")),
                                _=>{},
                            }
                            if sequenced{send(&mut connection,client,grant,&flow.acknowledgement(),&mut outgoing,clock).await?;}
                        }
                        Ok(RelayMessage::Ok(response))if !response.accepted=>return Err(failure(format!("relay rejected terminal frame: {}",response.message))),
                        Ok(RelayMessage::Closed{message,..})=>return Err(failure(message)),
                        Err(WsClientError::Timeout)=>{},Err(error)=>return Err(failure(error)),_=>{},
                    }
                }
                _=tick.tick()=>{
                    if now()>=grant.expires_at()||activity.elapsed()>=Duration::from_secs(300){return Ok(true);}
                    if refresh.elapsed()>=Duration::from_secs(5){
                        authorized(client,owner,relay,pending,grant).await?;refresh=Instant::now();
                    }
                    for frame in flow.retries(clock.elapsed().as_millis() as u64).map_err(failure)?{
                        send(&mut connection,client,grant,&frame,&mut outgoing,clock).await?;
                    }
                    if raw.is_some()&&flow.writable(){
                        let current=dimensions()?;
                        let packet=if current!=size{size=current;Some(Packet::Resize{rows:size.0,cols:size.1})}
                            else if heartbeat.elapsed()>=Duration::from_secs(5){heartbeat=Instant::now();Some(Packet::data(&[]).map_err(failure)?)}else{None};
                        if let Some(packet)=packet{let frame=flow.send(packet,clock.elapsed().as_millis() as u64).map_err(failure)?;send(&mut connection,client,grant,&frame,&mut outgoing,clock).await?;}
                    }
                }
            }
            Ok(false)
        }.await;
        match step {
            Ok(false) => {}
            Ok(true) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    // Best-effort immediate close; the server independently bounds lifetime, authority and idle.
    if flow.writable() {
        if let Ok(frame) = flow.send(Packet::Close, clock.elapsed().as_millis() as u64) {
            let _ = send(&mut connection, client, grant, &frame, &mut outgoing, clock).await;
        }
    }
    let restored = raw.map(Raw::restore).transpose();
    eprintln!("\r\nSandbox terminal closed.");
    match (outcome, restored) {
        (Err(error), Err(restore)) => Err(failure(format!(
            "{error}; TTY restoration failed: {restore}"
        ))),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

fn split_input(bytes: &[u8]) -> (&[u8], bool) {
    match bytes.iter().position(|byte| *byte == 0x1d) {
        Some(index) => (&bytes[..index], true),
        None => (bytes, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_input_preserves_control_bytes_and_detach_is_local() {
        let bytes = b"\x03\x1b[A\x00paste\r\n";
        assert_eq!(split_input(bytes), (bytes.as_slice(), false));
        assert_eq!(
            split_input(b"before\x1dnever sent"),
            (b"before".as_slice(), true)
        );
    }
}
