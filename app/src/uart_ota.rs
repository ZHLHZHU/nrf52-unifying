//! Numbered, stop-and-wait UART DFU. CRC32 detects corruption, not authenticity.
pub const LIMIT: usize = 480 * 1024;
pub trait Storage {
    fn write(&mut self, offset: usize, bytes: &[u8]) -> bool;
    fn read(&mut self, offset: usize, bytes: &mut [u8]) -> bool;
    fn commit(&mut self) -> bool;
}
pub fn crc_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes { crc ^= b as u32; for _ in 0..8 { crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1)); } }
    crc
}
fn hex(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 8 { return None; }
    s.iter().try_fold(0u32, |v, b| Some((v << 4) | match b { b'0'..=b'9' => (b-b'0') as u32, b'A'..=b'F' => (b-b'A'+10) as u32, b'a'..=b'f' => (b-b'a'+10) as u32, _ => return None }))
}
#[derive(Default)]
pub struct Ota { pub active: bool, size: usize, received: usize, expected_crc: u32 }
impl Ota {
    pub fn abort(&mut self) { self.active = false; self.received = 0; }
    pub fn handle<S: Storage>(&mut self, line: &[u8], storage: &mut S) -> (&'static [u8], bool) {
        let mut t = line.split(|&x| x == b' ');
        if t.next() != Some(b"OTA") { return (b"ERR CMD\r\n", false); }
        match t.next() {
            Some(b"CAP") if t.next().is_none() => (b"OTA UART1 BLOCK64 CRC32\r\n",false),
            Some(b"BEGIN") => {
                let (Some(size), Some(crc)) = (t.next().and_then(hex), t.next().and_then(hex)) else { return (b"ERR ARG\r\n", false); };
                if t.next().is_some() || size < 256 || size as usize > LIMIT || size % 4 != 0 { return (b"ERR SIZE\r\n", false); }
                self.size = size as usize; self.expected_crc = crc; self.received = 0; self.active = true;
                (b"READY\r\n", false)
            }
            Some(b"DATA") => {
                if !self.active { return (b"ERR NOIMAGE\r\n", false); }
                let (Some(offset), Some(payload)) = (t.next().and_then(hex), t.next()) else { return (b"ERR ARG\r\n", false); };
                if t.next().is_some() || payload.is_empty() || payload.len() > 128 || payload.len() % 8 != 0 { return (b"ERR ARG\r\n", false); }
                let mut bytes = [0u8;64]; let n = payload.len()/2;
                for (i, pair) in payload.chunks(2).enumerate() { let Some(v) = hex(pair) else { return (b"ERR ARG\r\n", false); }; bytes[i]=v as u8; }
                let offset = offset as usize;
                if offset.checked_add(n).is_none_or(|end| end > self.size) { return (b"ERR OFFSET\r\n", false); }
                // Only identical already-accepted chunks may be retried after a lost ACK.
                if offset < self.received {
                    let mut old=[0u8;64];
                    return (if offset+n <= self.received && storage.read(offset,&mut old[..n]) && old[..n]==bytes[..n] { b"OK\r\n" } else { b"ERR OFFSET\r\n" }, false);
                }
                if offset != self.received { return (b"ERR OFFSET\r\n", false); }
                if offset == 0 {
                    if n < 8 { return (b"ERR VECTOR\r\n", false); }
                    let sp=u32::from_le_bytes(bytes[0..4].try_into().unwrap());
                    let reset=u32::from_le_bytes(bytes[4..8].try_into().unwrap());
                    if !(0x20000000..=0x20040000).contains(&sp) || sp%8!=0 || reset&1==0 || !(0x7100..0x7000+self.size as u32).contains(&(reset&!1)) { return (b"ERR VECTOR\r\n",false); }
                }
                if !storage.write(offset,&bytes[..n]) { self.abort(); return (b"ERR WRITE\r\n",false); }
                self.received += n; (b"OK\r\n",false)
            }
            Some(b"COMMIT") if t.next().is_none() => {
                if !self.active || self.received != self.size { return (b"ERR INCOMPLETE\r\n",false); }
                let mut crc=!0; let mut bytes=[0u8;64];
                for offset in (0..self.size).step_by(64) { let n=(self.size-offset).min(64); if !storage.read(offset,&mut bytes[..n]) { self.abort(); return (b"ERR READ\r\n",false); } crc=crc_update(crc,&bytes[..n]); }
                if !crc != self.expected_crc { self.abort(); return (b"ERR CRC\r\n",false); }
                if !storage.commit() { self.abort(); return (b"ERR MARK\r\n",false); }
                self.active=false; (b"REBOOT\r\n",true)
            }
            Some(b"ABORT") if t.next().is_none() => { self.abort(); (b"ABORTED\r\n",false) }
            _ => (b"ERR ARG\r\n",false),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Mem { bytes: [u8;512], committed:bool }
    impl Storage for Mem { fn write(&mut self,o:usize,b:&[u8])->bool { self.bytes[o..o+b.len()].copy_from_slice(b);true } fn read(&mut self,o:usize,b:&mut[u8])->bool { b.copy_from_slice(&self.bytes[o..o+b.len()]);true } fn commit(&mut self)->bool {self.committed=true;true} }
    fn setup()->(Ota,Mem) {(Ota::default(),Mem{bytes:[255;512],committed:false})}
    fn image()->[u8;512] {let mut b=[0u8;512];b[..4].copy_from_slice(&0x20040000u32.to_le_bytes());b[4..8].copy_from_slice(&0x7101u32.to_le_bytes());b}
    fn upload(ota:&mut Ota,m:&mut Mem,b:&[u8]) {for (i,c) in b.chunks(64).enumerate(){let h:String=c.iter().map(|x|format!("{x:02X}")).collect();assert_eq!(ota.handle(format!("OTA DATA {:08X} {h}",i*64).as_bytes(),m).0,b"OK\r\n");}}
    #[test] fn crc_known_vector(){assert_eq!(!crc_update(!0,b"123456789"),0xcbf43926);}
    #[test] fn complete_image_commits(){let (mut o,mut m)=setup();let b=image();let c=!crc_update(!0,&b);o.handle(format!("OTA BEGIN 00000200 {c:08X}").as_bytes(),&mut m);upload(&mut o,&mut m,&b);assert_eq!(o.handle(b"OTA COMMIT",&mut m),(b"REBOOT\r\n" as &[u8],true));assert!(m.committed);}
    #[test] fn corrupt_crc_never_commits(){let(mut o,mut m)=setup();o.handle(b"OTA BEGIN 00000200 00000000",&mut m);upload(&mut o,&mut m,&image());assert_eq!(o.handle(b"OTA COMMIT",&mut m).0,b"ERR CRC\r\n");assert!(!m.committed);}
    #[test] fn partial_image_never_commits(){let(mut o,mut m)=setup();o.handle(b"OTA BEGIN 00000200 00000000",&mut m);assert_eq!(o.handle(b"OTA COMMIT",&mut m).0,b"ERR INCOMPLETE\r\n");assert!(!m.committed);}
    #[test] fn reject_bad_size_offset_vector(){let(mut o,mut m)=setup();assert_eq!(o.handle(b"OTA BEGIN FFFFFFFF 00000000",&mut m).0,b"ERR SIZE\r\n");o.handle(b"OTA BEGIN 00000200 00000000",&mut m);assert_eq!(o.handle(b"OTA DATA 00000004 0000000000000000",&mut m).0,b"ERR OFFSET\r\n");assert_eq!(o.handle(b"OTA DATA 00000000 0000000000000000",&mut m).0,b"ERR VECTOR\r\n");}
    #[test] fn identical_retry_only(){let(mut o,mut m)=setup();o.handle(b"OTA BEGIN 00000200 00000000",&mut m);let b=image();let h:String=b[..64].iter().map(|x|format!("{x:02X}")).collect();let cmd=format!("OTA DATA 00000000 {h}");assert_eq!(o.handle(cmd.as_bytes(),&mut m).0,b"OK\r\n");assert_eq!(o.handle(cmd.as_bytes(),&mut m).0,b"OK\r\n");assert_eq!(o.received,64);assert_eq!(o.handle(b"OTA DATA 00000000 0000000000000000",&mut m).0,b"ERR OFFSET\r\n");}
}
