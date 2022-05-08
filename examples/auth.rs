use std::io::SeekFrom;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_watch::{Result, WatchedFile};
use std::net::{IpAddr};
use serde::Deserialize;
use reqwest::{Url,Client};
// use reqwest::Url;


#[derive(Deserialize,Debug)]
struct AbuseIpResponse{
    ipAddress : IpAddr,
    abuseConfidenceScore : f32,
    countryCode : String,
    totalReports : usize,
    numDistinctUsers : usize
}

#[derive(Deserialize,Debug)]
struct ResponseWrapper{
    data : AbuseIpResponse
}

#[tokio::main]
async fn main() -> Result<()> {
    let file = WatchedFile::new("/var/log/auth.log").await?;
    let abuse_ip_key = tokio::fs::read_to_string("./abuseip.key").await?;
    let handle = tokio::spawn(async move {
        let mut ip_to_count = std::collections::hash_map::HashMap::<IpAddr, usize>::new();
        let mut file = BufReader::new(file).lines();
        let client = Client::new();
        while let Some(line) = file.next_line().await.unwrap() {
            if  line.contains("Failed") {
                // println!("{}", line);
                if let Some((_, identifier)) = line.split_once(" for ") {
                    if let Some((user, ip)) = identifier.split_once(" from ") {
                        if let Some((ip,_)) = ip.split_once(' ') {
                            let user = match user.strip_prefix("invalid user ") {
                                Some(new_user) => new_user,
                                None => user
                            };
                            let ip = ip.parse().unwrap();
                            let count = {
                                if let Some(count) = ip_to_count.get_mut(&ip) {
                                    *count += 1;
                                    *count 
                                } else {
                                    ip_to_count.insert(ip, 1);
                                    let url = Url::parse_with_params("https://api.abuseipdb.com/api/v2/check",&[("ipAddress", format!("{:?}",ip).as_str()), ("maxAgeInDays", "90")]).unwrap();
                                    let response = client.get(url).header("Key", abuse_ip_key.as_str())
                                        .header("Accept", "application/json").send().await.unwrap().json::<ResponseWrapper>().await.unwrap().data;
                                    println!("get response {:?}", response);
                                    1
                                }
                            };
                            println!("{:?} failed to login as \"{}\". This is infraction {}", ip, user, count);
                            continue;
                        }
                    }
                }
                eprintln!("failed to parse line with Failed: {}", line);
            }
        }
    });
    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal?;
        },
        h = handle => {
            h?;
        }
    }
    Ok(())
}
