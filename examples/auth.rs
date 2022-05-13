use reqwest::{Client, Url};
use serde::Deserialize;
use std::io::BufRead;
use std::net::IpAddr;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::channel;
use tokio_watch::{Result, WatchedFile};
// use reqwest::Url;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

#[derive(Deserialize, Debug)]
struct AbuseIpResponse {
    ipAddress: IpAddr,
    abuseConfidenceScore: u8,
    countryCode: String,
    totalReports: usize,
    numDistinctUsers: usize,
}

#[derive(Deserialize, Debug)]
struct ResponseWrapper {
    data: AbuseIpResponse,
}

#[derive(Debug)]
struct IpMessage {
    ip: IpAddr,
    source: &'static str,
}

#[derive(Clone)]
struct IpDatabse {
    db: Arc<Mutex<std::collections::hash_map::HashMap<IpAddr, (usize, Option<AbuseIpResponse>)>>>,
    client: Client,
    key: String,
}

impl IpDatabse {
    async fn add_ip(&mut self, ip: IpAddr) -> usize {
        {
            let mut db = self.db.lock().unwrap();
            if let Some((count, _)) = db.get_mut(&ip) {
                *count += 1;
                return *count;
            } else {
                db.insert(ip, (1, None));
            }
        } //release lock
        let client = &self.client;
        let key = self.key.as_str();
        let response = {
            let url = Url::parse_with_params(
                "https://api.abuseipdb.com/api/v2/check",
                &[
                    ("ipAddress", format!("{:?}", ip).as_str()),
                    ("maxAgeInDays", "90"),
                ],
            )
            .unwrap();
            let response = client
                .get(url)
                .header("Key", key)
                .header("Accept", "application/json")
                .send()
                .await
                .unwrap()
                .json::<ResponseWrapper>()
                .await
                .unwrap()
                .data;
            response
        };
        println!("got response {:?}", response);
        let mut db = self.db.lock().unwrap();
        db.get_mut(&ip).unwrap().1 = Some(response);
        return 1;
    }

    fn to_csv(&self, path: impl AsRef<Path>) -> Result<()> {
        let mut file = std::fs::File::create(path)?;
        let db = self.db.lock().unwrap();
        writeln!(
            file,
            "ip,count,abuse_score,country_code,total_reports,num_distinct_users\n"
        )?;
        for (ip, (count, response)) in db.iter() {
            if let Some(response) = response {
                writeln!(
                    file,
                    "{:?},{},{},{},{},{}",
                    ip,
                    count,
                    response.abuseConfidenceScore,
                    response.countryCode,
                    response.totalReports,
                    response.numDistinctUsers
                )?;
            }
        }
        Ok(())
    }

    fn from_csv(path: impl AsRef<Path>) -> Result<Self> {
        let this = IpDatabse {
            db: Arc::new(Mutex::new(std::collections::hash_map::HashMap::new())),
            client: Client::new(),
            key: std::fs::read_to_string("./abuseip.key")?,
        };
        {
            let mut hash = this.db.lock().unwrap();
            let lines = std::io::BufReader::new(std::fs::File::open(path)?).lines();
            for line in lines.skip(2) {
                let line = line?;
                let mut split = line.as_str().split(',');
                let ip: IpAddr = split.next().unwrap().parse()?;
                let count: usize = split.next().unwrap().parse()?;
                let abuse_score: u8 = split.next().unwrap().parse()?;
                let country_code = split.next().unwrap().to_owned();
                let total_reports = split.next().unwrap().parse()?;
                let n_distinct = split.next().unwrap().parse()?;
                let abuse_response = AbuseIpResponse {
                    ipAddress: ip,
                    abuseConfidenceScore: abuse_score,
                    countryCode: country_code,
                    totalReports: total_reports,
                    numDistinctUsers: n_distinct,
                };
                hash.insert(ip, (count, Some(abuse_response)));
            }
        }
        Ok(this)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let ip_db = IpDatabse::from_csv("abuse_db_2.csv")?;
    let (tx, mut rx): (
        tokio::sync::mpsc::Sender<IpMessage>,
        tokio::sync::mpsc::Receiver<IpMessage>,
    ) = channel(16);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let ip_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {return ip_db;}
                ip_msg = rx.recv() => {
                    match ip_msg {
                        None => {return ip_db;}
                        Some(ip_msg) => {
                            let ip = ip_msg.ip;
                            let source = ip_msg.source;
                            let mut clone = ip_db.clone();
                            let count = tokio::spawn(async move {clone.add_ip(ip).await } ).await.unwrap();
                            println!("[{}] {:?} failed to login. This is infraction {}", source, ip, count);
                        }
                    }
                }
            }
        }
    });
    let tx2 = tx.clone();
    let _handle = tokio::spawn(async move {
        let mut file = BufReader::new(WatchedFile::new("/var/log/auth.log").await.unwrap()).lines();
        while let Some(line) = file.next_line().await.unwrap() {
            if line.contains(" Failed password for ") {
                // println!("{}", line);
                if let Some((_, identifier)) = line.split_once(" for ") {
                    if let Some((user, ip)) = identifier.rsplit_once(" from ") {
                        if let Some((ip, _)) = ip.split_once(' ') {
                            let _user = match user.strip_prefix("invalid user ") {
                                Some(new_user) => new_user,
                                None => user,
                            };
                            let ip = ip.parse().unwrap();
                            tx2.send(IpMessage {
                                ip,
                                source: "auth.log",
                            })
                            .await;
                            continue;
                        }
                    }
                }
                eprintln!("failed to parse line with Failed: {}", line);
            }
        }
    });
    let _mail = tokio::spawn(async move {
        let mut file =
            BufReader::new(WatchedFile::new("/var/log/mail.log").await.unwrap()).lines();
        while let Some(line) = file.next_line().await.unwrap() {
            if line.contains(": SASL PLAIN authentication failed:")
                || line.contains(": SASL LOGIN authentication failed:")
            {
                // println!("{}", line);
                if let Some((_, identifier)) = line.split_once(": warning: unknown[") {
                    if let Some((ip, _)) = identifier.rsplit_once("]") {
                        let ip = ip.parse().unwrap();
                        tx.send(IpMessage {
                            ip,
                            source: "mail.log.1",
                        })
                        .await;
                        continue;
                    }
                }
                eprintln!("failed to parse line with Failed: {}", line);
            }
        }
    });

    tokio::signal::ctrl_c().await?;
    println!("shutting down...");
    
    let db = ip_handle.await?;
    db.to_csv("abuse_db_2.csv")?;
    Ok(())
}
