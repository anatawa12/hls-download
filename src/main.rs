use std::env;
use m3u8_rs::playlist::{Playlist, MediaSegment};
use reqwest::Url;
use block_modes::BlockMode;
use std::fs::File;
use std::io::{Write, BufWriter};
use tokio::time::{Instant, Duration, delay_for};
use std::process::{Command, Stdio};
use indicatif::{ProgressBar, ProgressStyle};

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        panic!("{} <url> <key in hex or base64> <output path>", args[0])
    }
    let a = &args[1];
    let input_url = &Url::parse(&a).unwrap();
    let key: Option<[u8; 16]> = {
        let key_str: Vec<char> = args[2].chars().collect();
        // 22, 24
        match key_str.len() {
            0 => None,
            16 => {
                let mut key: [u8; 16] = [0; 16];

                for (i, c) in key_str.iter().enumerate() {
                    key[i] = *c as u8;
                }
                Some(key)
            }
            22 | 24 => {
                let decoded = base64::decode(&args[2]).unwrap();
                if decoded.len() != 16 {
                    panic!("decoded length of base64 is not 16")
                }

                let mut key: [u8; 16] = [0; 16];

                for (i, c) in decoded.iter().enumerate() {
                    key[i] = *c;
                }
                Some(key)
            }
            _ => panic!("key's length must be either 0, 16, 22, 24")
        }
    };
    let output_name = &args[3];

    let base_dir = tempdir::TempDir::new("hls-download").unwrap();
    let base_path = base_dir.path();
    //let base_path = std::path::Path::new(".");

    println!("using base_dir: {}", base_path.display());

    let client = reqwest::Client::new();
    let resp = client.get(input_url.as_str())
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let list = m3u8_rs::parse_playlist(&resp).unwrap();
    match list {
        (_, Playlist::MasterPlaylist(pl)) => {
            println!("it's master playlist");
            let variant = &pl.variants[0];
            let media_list_url = input_url.join(variant.uri.as_str()).unwrap();
            println!("media list: {}", media_list_url);
            let resp = client.get(media_list_url.as_str())
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .to_owned();
            let (_, list) = m3u8_rs::parse_media_playlist(&resp).unwrap();
            process_media_list(&client, &base_path, output_name, &media_list_url, &key, &list).await;
        }
        (_, Playlist::MediaPlaylist(pl)) => {
            println!("it's media playlist");
            process_media_list(&client, &base_path, output_name, &input_url, &key, &pl).await;
        }
    }

    base_dir.close().unwrap();
}

async fn process_media_list(
    client: &reqwest::Client,
    base_path: &std::path::Path,
    output_name: &str,
    url: &Url,
    key: &Option<[u8; 16]>,
    list: &m3u8_rs::playlist::MediaPlaylist,
) {
    type AesCbc = block_modes::Cbc<aes::Aes128, block_modes::block_padding::Pkcs7>;

    let mut new_list = m3u8_rs::playlist::MediaPlaylist::default();
    new_list.version = list.version;
    new_list.target_duration = list.target_duration;
    new_list.end_list = true;

    let mut iv_opt: Option<Vec<u8>> = None;
    let mut last = tokio::time::Instant::now();
    println!("found {} medias in segment", list.segments.len());
    let progress = ProgressBar::new(list.segments.len() as u64);
    progress.set_style(ProgressStyle::default_bar().template("[{elapsed_precise}] {bar:40blue} {pos:>7}/{len:7} ({percent}%) {msg}")
        .progress_chars("##-"));
    progress.enable_steady_tick(100);
    for (i, segment) in list.segments.iter().enumerate() {
        match key {
            None => {
                if segment.key != None {
                    panic!("key attribute specified but no key specified")
                }
            }
            Some(_) => {
                iv_opt = Some(match &segment.key {
                    Some(v) => {
                        let iv_str = v.iv.as_ref().unwrap();
                        if !iv_str.starts_with("0x") && !iv_str.starts_with("0X") {
                            panic!("invalid iv: not starts with 0x")
                        }
                        let vec = hex::decode(&iv_str[2..]).unwrap();
                        if vec.len() != 16 {
                            panic!("invalid iv: length")
                        }
                        vec
                    },
                    None => iv_opt.unwrap()
                });
            }
        }
        progress.set_position(i as u64);
        progress.set_message(format!("processing {}", i));
        async fn download(client: &reqwest::Client, url: &Url, segment: &MediaSegment) -> bytes::Bytes {
            client.get(url.join(&segment.uri).unwrap().as_str())
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        }

        let decoded = match key {
            None => download(client, url, segment).await.to_vec(),
            Some(key) => {
                let decoded;
                loop {
                    let cipher = AesCbc::new_from_slices(key, iv_opt.as_ref().unwrap().as_slice()).unwrap();
                    if let Ok(value) = cipher.decrypt_vec(&download(client, url, segment).await) {
                        decoded = value;
                        break;
                    }
                }
                decoded
            }
        };

        let file_name = format!("{}{}{}.ts", base_path.display(), std::path::MAIN_SEPARATOR, i);
        let mut file = File::create(base_path.join(&file_name)).unwrap();
        file.write_all(&decoded).unwrap();
        file.flush().unwrap();

        new_list.segments.push(MediaSegment {
            uri: file_name,
            duration: segment.duration,
            title: None,
            byte_range: None,
            discontinuity: false,
            key: None,
            map: None,
            program_date_time: None,
            daterange: None,
            unknown_tags: vec![]
        });

        let mut delay_until = last + Duration::from_secs(2);
        let now = Instant::now();
        if delay_until <= now {
            let dur = now - last;
            progress.println(&format!("warn: process take {}.{:>04}s", dur.as_secs(), dur.subsec_millis()));
            while delay_until <= now {
                delay_until += Duration::new(1, 0);
            }
        }
        if delay_until > now {
            delay_for(delay_until - now).await;
        } else {
            progress.println("warn: process take 1 or more seconds");
        }
        last = Instant::now();
    }

    progress.finish();

    println!("writing m3u8");
    let file_name = base_path.join("main.m3u8");
    let mut writer = BufWriter::new(File::create(&file_name).unwrap());
    new_list.write_to(&mut writer).unwrap();
    writer.flush().unwrap();

    Command::new("ffmpeg")
        .arg("-loglevel").arg("warning") // quiet
        .arg("-i").arg(&file_name.as_os_str())// input file
        .arg("-movflags").arg("faststart") // metadata at first
        .arg("-codec:a").arg("copy") // audio codec: copy, no re-encoding
        .arg("-codec:v").arg("copy") // video codec: copy, no re-encoding
        .arg(output_name) // output file name
        .stdin(Stdio::null())
        .status()
        .unwrap();
}
