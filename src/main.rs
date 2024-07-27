use clap::{command, Parser};
use reqwest::Url;
use std::{
    io::{Error, ErrorKind, Read, Seek, Write},
    sync::mpsc::Sender,
    thread::spawn,
};

enum TaskResult {
    Downloading(usize, u64, Box<[u8]>),
    Failed(usize, Error),
    Done(usize),
}

#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    #[clap(long, short, default_value = "2")]
    threads: usize,

    #[clap(long, short)]
    output: Option<String>,

    #[clap(long, short, default_value = "false")]
    verbose: bool,

    url: String,
}

fn get_file_size(url: &str) -> Result<u64, Error> {
    let response = reqwest::blocking::Client::new()
        .head(url)
        .header(reqwest::header::USER_AGENT, "curl/7.81.0")
        .send()
        .map_err(|e| Error::new(ErrorKind::ConnectionReset, e))?;

    if !response.status().is_success() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("Failed to get content-length: {}", response.status()),
        ));
    }
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "Failed to parse content-length"))
}

fn download_part(tx: Sender<TaskResult>, url: String, idx: usize, pos: u64, length: u64) -> u64 {
    match download_part_inner(tx.clone(), url, idx, pos, length) {
        Ok(pos) => {
            tx.send(TaskResult::Done(idx)).ok();
            pos
        }
        Err(e) => {
            tx.send(TaskResult::Failed(idx, e)).ok();
            0
        }
    }
}

fn download_part_inner(
    tx: Sender<TaskResult>,
    url: String,
    idx: usize,
    pos: u64,
    length: u64,
) -> Result<u64, Error> {
    let client = reqwest::blocking::Client::new();
    let mut response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, "curl/7.81.0")
        .header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", pos, pos + length - 1),
        )
        .send()
        .map_err(|e| Error::new(ErrorKind::ConnectionReset, e))?;

    if !response.status().is_success() {
        let status = response.status();
        let reason = response.text().unwrap_or(format!("{}", status));
        return Err(Error::new(ErrorKind::InvalidData, reason));
    }

    let mut pos = pos;
    loop {
        let mut buffer = [0u8; 8 * 1024];
        let n = response.read(&mut buffer)?;
        if n == 0 {
            return Ok(pos);
        }

        tx.send(TaskResult::Downloading(
            idx,
            pos,
            buffer[..n].to_vec().into_boxed_slice(),
        ))
        .map_err(|_| Error::new(ErrorKind::InvalidData, "Failed to send download event"))?;
        pos += n as u64;
    }
}

fn download(
    url: &str,
    output: Option<String>,
    threads: usize,
    verbose: bool,
) -> Result<String, Error> {
    let file_name = match output {
        Some(name) => name.to_string(),
        None => {
            let url = Url::parse(url).map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
            url.path_segments()
                .and_then(|segments| segments.last())
                .and_then(|name| if name.is_empty() { None } else { Some(name) })
                .unwrap_or("index.html")
                .to_string()
        }
    };

    let file_size = match get_file_size(url)? {
        0 => return Err(Error::new(ErrorKind::InvalidData, "File size is 0")),
        file_size => file_size,
    };
    // try rename the file to avoid conflict
    let mut file_name = file_name;
    let mut index = 1;
    while std::fs::metadata(&file_name).is_ok() {
        let parts: Vec<&str> = file_name.rsplitn(2, '.').collect();
        if parts.len() == 2 {
            file_name = format!("{}.{}.{}", parts[1], index, parts[0]);
        } else {
            file_name = format!("{}.{}", file_name, index);
        }
        index += 1;
    }
    if verbose {
        println!(
            "Downloading {} to {} with {} threads, content-length: {}",
            url, file_name, threads, file_size
        );
    }

    let threads = std::cmp::max(threads, 1);
    let (tx, rx) = std::sync::mpsc::channel::<TaskResult>();
    let mut done_count = 0;

    for idx in 0..threads {
        let pos = idx as u64 * file_size / threads as u64;
        let length = if idx == threads - 1 {
            file_size - pos
        } else {
            file_size / threads as u64
        };
        let url = url.to_string();
        let tx = tx.clone();
        if verbose {
            println!("Thread {} start: pos={} length={}", idx, pos, length);
        }
        spawn(move || download_part(tx.clone(), url, idx, pos, length));
    }

    let start_time = std::time::Instant::now();
    let mut downloaded = 0;

    let mut outfile = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(&file_name)
        .map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;

    loop {
        match rx.recv() {
            Ok(TaskResult::Downloading(_idx, pos, data)) => {
                downloaded += data.len() as u64;
                if verbose {
                    let percent = 100 * (downloaded / file_size);
                    let filled_length = 50 * (downloaded / file_size);
                    let bar = "█".repeat(filled_length as usize)
                        + &"-".repeat((50 - filled_length) as usize);
                    print!("\rProgress: |{}| {}% Complete", bar, percent);
                    std::io::stdout().flush().ok();
                    if downloaded == file_size {
                        println!();
                    }
                }
                outfile.seek(std::io::SeekFrom::Start(pos))?;
                outfile.write_all(&data)?;
            }
            Ok(TaskResult::Failed(idx, e)) => {
                println!("Thread {} failed: {}", idx, e);
                return Err(e);
            }
            Ok(TaskResult::Done(_idx)) => {
                done_count += 1;
                if done_count == threads {
                    break;
                }
            }
            Err(e) => {
                return Err(Error::new(ErrorKind::InvalidData, e));
            }
        }
    }

    let elapsed = start_time.elapsed();
    if verbose {
        println!(
            "Downloaded {} bytes in {} seconds, speed: {:.2} MB/s",
            file_size,
            elapsed.as_secs_f32(),
            file_size as f32 / 1024.0 / 1024.0 / elapsed.as_secs_f32()
        );
    }
    outfile.flush().ok();
    Ok(file_name)
}

// a multiple threads downloader
// by ruzhila.cn
fn main() {
    let args = Cli::parse();
    match download(&args.url, args.output.clone(), args.threads, args.verbose) {
        Ok(filename) => println!("Downloaded successfully: {}", filename),
        Err(e) => eprintln!("Error: {}", e),
    }
}
