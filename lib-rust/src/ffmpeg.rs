use crate::config::HypetriggerConfig;
use crate::logging::LoggingConfig;
use crate::runner::{ProcessImagePayload, RunnerCommand, WorkerThread};
use crate::trigger::Trigger;

use std::io::{BufRead, BufReader, Error, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{mpsc::Receiver, Arc};
use std::thread;
use std::thread::JoinHandle;

pub type RawImageData = Arc<Vec<u8>>;

pub enum FfmpegStdinCommand {
    Stop,
}

/// Specifies whether to attach to each stdio channel or not
pub struct StdioConfig {
    pub stdin: Stdio,
    pub stdout: Stdio,
    pub stderr: Stdio,
}

/// Generates and runs an FFMPEG command similar to this one (in the case of two inputs):
///
/// ```
/// ffmpeg \
///  -hwaccel cuda \
///  -i "F:/OBS/Road to the 20-Bomb/17.mp4" \
///  -filter_complex "[0:v]fps=2,split=2[in1][in2];[in1]crop=60.59988:60.59988:930.70038:885.6,scale=224:224,negate[out1];[in2]crop=2:2:0:0,scale=224:224[out2];[out1][out2]" \
///  -map "[out0]" \
///  -map "[out1]" \
///  -f rawvideo \
///  -pix_fmt rgb24 \
///  -an -y pipe:1 > "scripts/raw.bin"
/// ```
///
/// Explanation of all FFMPEG parameters:
/// - `-hwaccel cuda` (or `-hwaccel auto`) run on the GPU
/// - `-i path/to/file.mp4` reads the input video
/// - `-filter_complex` transforms every frame into the format expected by tesseract or tensorflow
///   - `fps=x` drops the fps to the sample rate, skipping all other frames
///   - `split=n` splits video for every trigger
///   - `crop` isolates the rectangle identified in trigger config `cropFunction`
///   - `scale` only applies to tensorflow, and resizes output to 224x224 expected by the NN
/// - `-map [outN]` creates one output stream for each branch in the filter graph
/// - `-vsync drop` *important* drops frame timestamps, needed for `interleave` filter to behave as expected
/// - `-f rawvideo` since no output file is specified, tell FFMPEG which video format to use (raw bytes)
/// - `-pix_fmt rgb24` 1 byte per pixel, 3 channels, RGB
/// - `-an` drop audio
/// - `-y` *unneccessary* overwrite output file if it exists (irrelevant in this case)
/// - `-pipe:1` output to stdout (this will be consumed on another thread for processing)
///
pub fn spawn_ffmpeg_childprocess(
    config: Arc<HypetriggerConfig>,
    stdio_config: StdioConfig,
) -> Result<Child, Error> {
    let input_video = config.inputPath.as_str();
    let samples_per_second = config.samplesPerSecond;
    let num_triggers = config.triggers.len();

    let mut filter_complex: String =
        format!("[0:v]fps={},split={}", samples_per_second, num_triggers);
    for i in 0..num_triggers {
        filter_complex.push_str(format!("[in{}]", i).as_str());
    }
    filter_complex.push(';');
    for i in 0..num_triggers {
        let trigger = &config.triggers[i];
        let in_w = trigger.get_crop().widthPercent / 100.0;
        let in_h = trigger.get_crop().heightPercent / 100.0;
        let x = trigger.get_crop().xPercent / 100.0;
        let y = trigger.get_crop().yPercent / 100.0;

        filter_complex.push_str(
            format!(
                "[in{}]crop=round(in_w*{}):round(in_h*{}):round(in_w*{}):round(in_h*{})[out{}]",
                i, in_w, in_h, x, y, i
            )
            .as_str(),
        );
        if i < num_triggers - 1 {
            filter_complex.push(';');
        }
    }

    let ffmpeg_path: PathBuf = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("ffmpeg.exe");
    let ffmpeg_path_str: &str = ffmpeg_path.as_os_str().to_str().to_owned().unwrap();

    if config.logging.debug_ffmpeg {
        println!("[ffmpeg] exe: {}", ffmpeg_path_str);
    }

    let mut cmd = Command::new(ffmpeg_path_str);
    cmd.arg("-hwaccel")
        .arg("auto")
        .arg("-i")
        .arg(input_video)
        .arg("-filter_complex")
        .arg(filter_complex.clone());

    for i in 0..num_triggers {
        cmd.arg("-map").arg(format!("[out{}]", i));
    }

    let child = cmd
        .arg("-vsync")
        .arg("drop")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgb24")
        .arg("-an")
        .arg("-y")
        .arg("pipe:1")
        .stdin(stdio_config.stdin)
        .stdout(stdio_config.stdout)
        .stderr(stdio_config.stderr)
        .creation_flags(0x08000000)
        .spawn();

    if config.logging.debug_ffmpeg {
        println!("[ffmpeg] debug command:");
        println!("ffmpeg \\");
        println!("  -hwaccel auto \\");
        println!("  -i \"{}\" \\", input_video);
        println!("  -filter_complex \"{}\" \\", filter_complex);
        for i in 0..num_triggers {
            println!("  -map [out{}] \\", i);
        }
        println!("  -vsync drop \\");
        println!("  -vframes {} \\", num_triggers * 5);
        println!("  -an -y \\");
        println!("  \"scripts/frame%03d.bmp\"");
    }

    child
}

/// Callback for each line of FFMPEG stderr
pub type OnFfmpegStderr = Arc<dyn Fn(Result<String, Error>) + Send + Sync>;

/// Optional thread to process stderr from ffmpeg. It will automatically terminate
/// when the ffmpeg process exits.
///
/// FFMPEG sends all logs to stderr (not necessarily just errors)
/// We pipe these and read them async to extract info like video duration,
/// or re-routing ffmpeg logs to println.
///
/// - Recieves: lines from ffmpeg stderr
/// - Sends: Nothing/calls callback on each line
pub fn spawn_ffmpeg_stderr_thread(
    stderr: ChildStderr,
    logging: LoggingConfig,
    on_ffmpeg_stderr: OnFfmpegStderr,
) -> Result<JoinHandle<()>, Error> {
    thread::Builder::new()
        .name("ffmpeg_stderr".into())
        .spawn(move || {
            BufReader::new(stderr)
                .lines()
                .for_each(|line| (on_ffmpeg_stderr)(line));
            if logging.debug_thread_exit {
                println!("[ffmpeg.stderr] done; thread exiting");
            }
        })
}

/// Callback for every line of ffmpeg stderr
pub fn on_ffmpeg_stderr(line: Result<String, Error>) {
    match line {
        Ok(string) => println!("{}", string),
        Err(error) => eprintln!("{}", error),
    }
}

/// Handles receiving raw pixel data from FFMPEG on the stdout channel
/// and mapping it to the corresponding trigger config.
pub fn spawn_ffmpeg_stdout_thread(
    mut stdout: ChildStdout,
    config: Arc<HypetriggerConfig>,
    on_ffmpeg_stdout: OnFfmpegStdout,
    get_runner: GetRunner,
) -> Result<JoinHandle<()>, Error> {
    thread::Builder::new()
        .name("ffmpeg_stdout".into())
        .spawn(move || {
            // Init buffers
            let mut buffers: Vec<Vec<u8>> = Vec::new();
            for trigger in &config.triggers {
                let width = trigger.get_crop().width;
                let height = trigger.get_crop().height;
                const CHANNELS: u32 = 3;
                let buf_size = (width * height * CHANNELS) as usize;
                if trigger.get_debug() && config.logging.debug_buffer_allocation {
                    println!(
                        "[rust] Allocated buffer of size {} for trigger id {}",
                        buf_size,
                        trigger.get_id()
                    );
                }
                buffers.push(vec![0_u8; buf_size]);
            }

            // Listen for data
            let mut cur_frame = 0;
            let num_triggers = config.triggers.len();
            while stdout
                .read_exact(&mut buffers[cur_frame % num_triggers])
                .is_ok()
            {
                let cur_trigger = &config.triggers[cur_frame % num_triggers];
                let clone = buffers[cur_frame % num_triggers].clone(); // Necessary?
                let raw_image_data: RawImageData = Arc::new(clone);

                on_ffmpeg_stdout(
                    config.clone(),
                    cur_trigger.clone(),
                    raw_image_data,
                    get_runner.clone(),
                );

                cur_frame += 1;
            }

            if config.logging.debug_thread_exit {
                println!("[ffmpeg] done; thread exiting");
            }
        })
}

pub type GetRunner = Arc<dyn (Fn(String) -> WorkerThread) + Sync + Send>;
pub type OnFfmpegStdout =
    Arc<dyn Fn(Arc<HypetriggerConfig>, Arc<dyn Trigger>, RawImageData, GetRunner) + Sync + Send>;
pub fn on_ffmpeg_stdout(
    config: Arc<HypetriggerConfig>,
    cur_trigger: Arc<dyn Trigger>,
    raw_image_data: RawImageData,
    get_runner: GetRunner,
) {
    // TODO num_triggers went out of scope
    // if config.logging.debug_buffer_transfer {
    //     println!(
    //         "[ffmpeg] read {} bytes for trigger {}",
    //         buffers[cur_frame % num_triggers].len(),
    //         cur_trigger.id
    //     );
    // }

    let tx_name = &cur_trigger.get_runner_type();
    let tx = get_runner(tx_name.clone()).tx.clone();

    if config.logging.debug_buffer_transfer {
        println!(
            "[ffmpeg] sending {} bytes to {} for trigger {}",
            raw_image_data.len(),
            tx_name,
            cur_trigger.get_id(),
        );
    }

    let payload = ProcessImagePayload {
        input_id: config.inputPath.clone(),
        image: raw_image_data,
        trigger: cur_trigger,
    };

    tx.send(RunnerCommand::ProcessImage(payload))
        .expect("send image buffer");
}

pub fn spawn_ffmpeg_stdin_thread(
    mut stdin: ChildStdin,
    rx: Receiver<FfmpegStdinCommand>,
) -> Result<JoinHandle<()>, Error> {
    thread::Builder::new()
        .name("ffmpeg_stdin".into())
        .spawn(move || {
            while let Ok(command) = rx.recv() {
                match command {
                    FfmpegStdinCommand::Stop => {
                        stdin.write_all(b"q").expect("write to ffmpeg stdin");
                    }
                }
            }
            // while let Ok(Stop) = rx.recv() {
            //     println!("[ffmpeg.stdin] Sending quit signal");
            //     stdin.write_all(b"q\n").expect("send quit signal");
            // }
        })
}

// pub fn _test() {
//     let config = HypetriggerConfig {
//         inputPath: "test".into(),
//         outputPath: "test".into(),
//         inputWidth: 100,
//         inputHeight: 100,
//         samplesPerSecond: 2f64,
//         triggers: vec![],
//         saveScreenshots: false,
//         logging: LoggingConfig::default(),
//     };

//     let ffmpeg_childprocess = spawn_ffmpeg_childprocess(&config).expect("ffmpeg_childprocess");
//     let ffmpeg_stdout = ffmpeg_childprocess.stdout.expect("ffmpeg_stdout");
//     let ffmpeg_stdin = ffmpeg_childprocess.stdin.expect("ffmpeg_stdin");

//     let (tx_tesseract, rx_tesseract) = sync_channel::<RawImageData>(0);
//     let (tx_tensorflow, rx_tensorflow) = sync_channel::<RawImageData>(0);
//     let (tx_ffmpeg_stdin, rx_ffmpeg_stdin) = sync_channel::<FfmpegStdinCommand>(0);

//     let runner = HypetriggerRunner {
//         tx_tesseract,
//         tx_tensorflow,
//     };

//     let ffmpeg_stdout_thread =
//         spawn_ffmpeg_stdout_thread(ffmpeg_stdout, config.clone(), Box::new(runner))
//             .expect("ffmpeg_stdout_thread");

//     let ffmpeg_stdin_thread = spawn_ffmpeg_stdin_thread(ffmpeg_stdin, rx_ffmpeg_stdin);

//     // let ffmpeg_stderr_thread = ...
// }
