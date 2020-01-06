use async_std::future::timeout;
use async_std::net::UdpSocket;

use failure::Error;

use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct FoundTV {
    addr: String,
    uuid: Option<String>,
}

pub async fn scan(deadline: Duration) -> Result<Vec<FoundTV>, Error> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let request = b"M-SEARCH * HTTP/1.1\r\n\
        HOST: 239.255.255.250:1900\r\n\
        MAN: \"ssdp:discover\"\r\n\
        MX: 2\r\n\
        ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
    sock.send_to(request, "239.255.255.250:1900").await?;
    let mut buf = vec![0u8; 2048];
    let mut deadline = deadline;
    let mut found_tvs = vec!();
    loop {
        let start_at = Instant::now();
        let (_, peer) = match timeout(deadline, sock.recv_from(&mut buf)).await {
            Ok(resp) => resp?,
            Err(_) => return Ok(found_tvs), // TimeoutError
        };
        deadline -= start_at.elapsed();

        let mut found = false;
        let mut uuid = None;
        for line in buf.split(|c| *c == b'\n') {
            if line.starts_with(b"Server:") && String::from_utf8_lossy(line).contains("LGE WebOS TV") {
                found = true;
            }

            let uuid_prefix = b"USN: uuid:";
            if line.starts_with(uuid_prefix) {
                for (part_ix, part) in line.splitn(4, |c| *c == b':').enumerate() {
                    if part_ix == 2 {
                        uuid = Some(String::from_utf8_lossy(part).to_string());
                    }
                }
            }
        }
        let addr = peer.ip().to_string();
        if found && found_tvs.iter().all(|tv| tv.addr != addr) {
            found_tvs.push(FoundTV {
                addr, uuid,
            });
        }
    }
}

//#[cfg(test)]
//mod test {
//    use super::scan;
//    use std::time::Duration;
//
//    #[async_std::test]
//    async fn test_scan() {
//        let found_tvs = scan(Duration::from_secs(5)).await.expect("Error happened");
//        println!("{:?}", &found_tvs);
//    }
//}
