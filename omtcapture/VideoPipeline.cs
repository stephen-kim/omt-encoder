using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;
using libomtnet;
using omtcapture.capture;
using V4L2;

namespace omtcapture
{
    internal sealed class VideoPipeline : IDisposable
    {
        private readonly SendCoordinator _coordinator;
        private readonly object _settingsLock = new();
        private CancellationTokenSource? _cts;
        private Thread? _thread;
        private VideoSettings _settings;
        private PreviewSettings _previewSettings = new();
        private readonly List<PreviewPipeline> _previewPipelines = new();
        private readonly object _previewLock = new();
        private volatile bool _restartRequested;
        private volatile bool _previewRestartRequested;
        private static readonly double TimestampTo100Ns = 10_000_000.0 / Stopwatch.Frequency;

        public VideoPipeline(SendCoordinator coordinator, VideoSettings settings)
        {
            _coordinator = coordinator;
            _settings = Clone(settings);
        }

        public void Start()
        {
            _cts = new CancellationTokenSource();
            _thread = new Thread(() => Run(_cts.Token))
            {
                IsBackground = true,
                Name = "VideoPipeline",
                Priority = ThreadPriority.AboveNormal
            };
            _thread.Start();
        }

        public void Update(VideoSettings settings)
        {
            lock (_settingsLock)
            {
                _settings = Clone(settings);
            }
            _restartRequested = true;
        }

        public void UpdatePreview(PreviewSettings settings)
        {
            lock (_previewLock)
            {
                _previewSettings = Clone(settings);
            }
            _previewRestartRequested = true;
        }

        public void Stop()
        {
            if (_cts == null)
            {
                return;
            }

            _cts.Cancel();
            _thread?.Join(TimeSpan.FromSeconds(2));
            _cts.Dispose();
            _cts = null;
            _thread = null;
            StopPreview();
        }

        public void Dispose()
        {
            Stop();
        }

        private void Run(CancellationToken token)
        {
            while (!token.IsCancellationRequested)
            {
                VideoSettings settings;
                lock (_settingsLock)
                {
                    settings = Clone(_settings);
                }

                try
                {
                    RunCapture(settings, token);
                }
                catch (Exception ex)
                {
                    Console.WriteLine($"Video pipeline error: {ex.Message}");
                }

                if (token.IsCancellationRequested)
                {
                    break;
                }

                if (_restartRequested)
                {
                    _restartRequested = false;
                    continue;
                }

                Thread.Sleep(200);
            }
        }

        private void RunCapture(VideoSettings settings, CancellationToken token)
        {
            OMTMediaFrame frame = new OMTMediaFrame
            {
                Type = OMTFrameType.Video,
                ColorSpace = OMTColorSpace.BT709
            };

            int desiredCodec;
            switch (settings.Codec)
            {
                case "UYVY":
                    desiredCodec = (int)OMTCodec.UYVY;
                    break;
                case "YUY2":
                    desiredCodec = (int)OMTCodec.YUY2;
                    break;
                case "NV12":
                    desiredCodec = (int)OMTCodec.NV12;
                    break;
                default:
                    Console.WriteLine("Codec not supported");
                    return;
            }

            frame.Width = settings.Width;
            frame.Height = settings.Height;
            frame.FrameRateN = settings.FrameRateN;
            frame.FrameRateD = settings.FrameRateD;
            frame.AspectRatio = (float)settings.Width / settings.Height;

            int desiredStride = GetStride(desiredCodec, settings.Width);
            CaptureFormat fmt = new CaptureFormat(desiredCodec, settings.Width, settings.Height, desiredStride, settings.FrameRateN, settings.FrameRateD);

            int frameCount = 0;
            long sentLength = 0;
            long fpsWindowStart = Stopwatch.GetTimestamp();
            int fpsWindowFrames = 0;

            using CaptureDevice capture = new V4L2Capture(settings.DevicePath, fmt);

            fmt = capture.Format;
            if (fmt.Codec == (int)V4L2Unmanaged.PIXEL_FORMAT_YUYV)
            {
                fmt.Codec = (int)OMTCodec.YUY2;
            }

            int outputCodec = desiredCodec;
            int outputWidth = settings.Width;
            int outputHeight = settings.Height;
            int outputStride = desiredStride;
            int outputFrameRateN = settings.FrameRateN;
            int outputFrameRateD = settings.FrameRateD;

            bool needsTransform = fmt.Width != settings.Width ||
                fmt.Height != settings.Height ||
                fmt.Codec != desiredCodec;
            bool throttleFps = fmt.FrameRateN * outputFrameRateD > outputFrameRateN * fmt.FrameRateD;

            frame.Codec = outputCodec;
            frame.Width = outputWidth;
            frame.Height = outputHeight;
            frame.Stride = outputStride;
            frame.FrameRateD = outputFrameRateD;
            frame.FrameRateN = outputFrameRateN;

            Console.WriteLine("Format: " + fmt.ToString());

            StartPreviewIfEnabled(fmt);

            capture.StartCapture();
            CaptureFrame captureFrame = new CaptureFrame();
            Process? transformProcess = null;
            Stream? transformInput = null;
            Stream? transformOutput = null;
            byte[] transformInputBuffer = Array.Empty<byte>();
            byte[] transformOutputBuffer = Array.Empty<byte>();
            GCHandle transformOutputHandle = default;
            long lastFrameTicks = 0;
            long frameIntervalTicks = TimeSpan.FromSeconds((double)outputFrameRateD / Math.Max(1, outputFrameRateN)).Ticks;

            if (needsTransform)
            {
                try
                {
                    transformProcess = StartVideoTransform(fmt, outputCodec, outputWidth, outputHeight, outputFrameRateN, outputFrameRateD);
                    transformInput = transformProcess.StandardInput.BaseStream;
                    transformOutput = transformProcess.StandardOutput.BaseStream;
                    int inputSize = GetFrameSize(fmt.Codec, fmt.Width, fmt.Height);
                    int outputSize = GetFrameSize(outputCodec, outputWidth, outputHeight);
                    transformInputBuffer = new byte[inputSize];
                    transformOutputBuffer = new byte[outputSize];
                    transformOutputHandle = GCHandle.Alloc(transformOutputBuffer, GCHandleType.Pinned);
                }
                catch (Exception ex)
                {
                    Console.WriteLine($"Video transform error: {ex.Message}");
                    transformProcess?.Dispose();
                    transformProcess = null;
                }
            }

            if (needsTransform && transformProcess == null)
            {
                Console.WriteLine("Video transform unavailable; sending native format.");
                outputCodec = fmt.Codec;
                outputWidth = fmt.Width;
                outputHeight = fmt.Height;
                outputStride = fmt.Stride;
                outputFrameRateN = fmt.FrameRateN;
                outputFrameRateD = fmt.FrameRateD;
                frame.Codec = outputCodec;
                frame.Width = outputWidth;
                frame.Height = outputHeight;
                frame.Stride = outputStride;
                frame.FrameRateN = outputFrameRateN;
                frame.FrameRateD = outputFrameRateD;
                frame.AspectRatio = (float)outputWidth / outputHeight;
                throttleFps = false;
            }

            while (!token.IsCancellationRequested && !_restartRequested)
            {
                if (_previewRestartRequested)
                {
                    RestartPreview(fmt);
                    _previewRestartRequested = false;
                }

                if (capture.GetNextFrame(ref captureFrame))
                {
                    if (throttleFps)
                    {
                        long throttleTicks = DateTime.UtcNow.Ticks;
                        if (throttleTicks - lastFrameTicks < frameIntervalTicks)
                        {
                            continue;
                        }
                        lastFrameTicks = throttleTicks;
                    }

                    frame.Data = captureFrame.Data;
                    frame.DataLength = captureFrame.Length;
                    frame.Timestamp = GetMonotonicTimestamp100ns();

                    if (transformProcess != null && transformInput != null && transformOutput != null)
                    {
                        int inputSize = transformInputBuffer.Length;
                        System.Runtime.InteropServices.Marshal.Copy(captureFrame.Data, transformInputBuffer, 0, inputSize);
                        transformInput.Write(transformInputBuffer, 0, inputSize);
                        if (!ReadExact(transformOutput, transformOutputBuffer, transformOutputBuffer.Length))
                        {
                            Console.WriteLine("Video transform error: short read");
                            break;
                        }

                        frame.Data = transformOutputHandle.AddrOfPinnedObject();
                        frame.DataLength = transformOutputBuffer.Length;
                        frame.Timestamp = GetMonotonicTimestamp100ns();
                    }

                    byte[] payload = new byte[frame.DataLength];
                    System.Runtime.InteropServices.Marshal.Copy(frame.Data, payload, 0, frame.DataLength);
                    _coordinator.EnqueueVideo(payload, frame.Width, frame.Height, frame.Stride, frame.Codec, frame.FrameRateN, frame.FrameRateD, frame.Timestamp);

                    fpsWindowFrames++;
                    frameCount += 1;
                    sentLength += frame.DataLength;
                    if (frameCount >= 60)
                    {
                        Console.WriteLine("Sent " + frameCount + " frames, " + sentLength + " bytes.");
                        frameCount = 0;
                        sentLength = 0;
                    }

                    long nowTicks = Stopwatch.GetTimestamp();
                    double elapsedSeconds = (nowTicks - fpsWindowStart) / (double)Stopwatch.Frequency;
                    if (elapsedSeconds >= 2.0)
                    {
                        double fps = fpsWindowFrames / elapsedSeconds;
                        Console.WriteLine($"Video FPS: {fps:F1} (sent {fpsWindowFrames} frames in {elapsedSeconds:F2}s)");
                        fpsWindowFrames = 0;
                        fpsWindowStart = nowTicks;
                    }

                    foreach (PreviewPipeline pipeline in _previewPipelines)
                    {
                        pipeline.SubmitFrame(captureFrame.Data, captureFrame.Length);
                    }
                }
            }

            if (transformOutputHandle.IsAllocated)
            {
                transformOutputHandle.Free();
            }
            if (transformProcess != null)
            {
                try
                {
                    if (!transformProcess.HasExited)
                    {
                        transformProcess.Kill(true);
                    }
                }
                catch
                {
                    // Ignore shutdown errors.
                }
                transformProcess.Dispose();
            }

            capture.StopCapture();
            StopPreview();
        }

        private static VideoSettings Clone(VideoSettings settings)
        {
            return new VideoSettings
            {
                Name = settings.Name,
                DevicePath = settings.DevicePath,
                Width = settings.Width,
                Height = settings.Height,
                FrameRateN = settings.FrameRateN,
                FrameRateD = settings.FrameRateD,
                Codec = settings.Codec
            };
        }

        private static PreviewSettings Clone(PreviewSettings settings)
        {
            return new PreviewSettings
            {
                Enabled = settings.Enabled,
                OutputDevice = settings.OutputDevice,
                OutputDevices = new List<string>(settings.OutputDevices),
                Width = settings.Width,
                Height = settings.Height,
                Fps = settings.Fps,
                PixelFormat = settings.PixelFormat
            };
        }

        private void StartPreviewIfEnabled(CaptureFormat fmt)
        {
            PreviewSettings settings;
            lock (_previewLock)
            {
                settings = Clone(_previewSettings);
            }
            if (!settings.Enabled)
            {
                return;
            }

            string inputPixelFormat = ResolveInputPixelFormat(fmt.Codec);
            List<string> outputs = settings.OutputDevices.Count > 0
                ? settings.OutputDevices
                : new List<string> { settings.OutputDevice };

            foreach (string output in outputs.Distinct())
            {
                PreviewSettings perOutput = Clone(settings);
                perOutput.OutputDevice = output;
                if (TryGetFramebufferSize(output, out int fbWidth, out int fbHeight))
                {
                    perOutput.Width = fbWidth;
                    perOutput.Height = fbHeight;
                }
                else if (perOutput.Width <= 0 || perOutput.Height <= 0)
                {
                    perOutput.Width = fmt.Width;
                    perOutput.Height = fmt.Height;
                }
                PreviewPipeline pipeline = new PreviewPipeline(perOutput, inputPixelFormat, fmt.Width, fmt.Height);
                pipeline.Start();
                _previewPipelines.Add(pipeline);
            }
        }

        private static int GetStride(int codec, int width)
        {
            return codec switch
            {
                (int)OMTCodec.NV12 => width,
                _ => width * 2
            };
        }

        private static int GetFrameSize(int codec, int width, int height)
        {
            return codec switch
            {
                (int)OMTCodec.NV12 => (width * height * 3) / 2,
                _ => width * height * 2
            };
        }

        private static long GetMonotonicTimestamp100ns()
        {
            return (long)(Stopwatch.GetTimestamp() * TimestampTo100Ns);
        }

        private static string GetPixelFormat(int codec)
        {
            return codec switch
            {
                (int)OMTCodec.UYVY => "uyvy422",
                (int)OMTCodec.YUY2 => "yuyv422",
                (int)OMTCodec.NV12 => "nv12",
                _ => "yuyv422"
            };
        }

        private static Process StartVideoTransform(CaptureFormat inputFormat, int outputCodec, int outputWidth, int outputHeight, int outputFrameRateN, int outputFrameRateD)
        {
            string inputPixFmt = GetPixelFormat(inputFormat.Codec);
            string outputPixFmt = GetPixelFormat(outputCodec);
            double inputFps = inputFormat.FrameRateD == 0 ? 30 : (double)inputFormat.FrameRateN / inputFormat.FrameRateD;
            string filter = $"scale={outputWidth}:{outputHeight}:flags=fast_bilinear,format={outputPixFmt}";
            string args = $"-loglevel error -f rawvideo -pix_fmt {inputPixFmt} -s {inputFormat.Width}x{inputFormat.Height} -r {inputFps:0.###} -i pipe:0 -vf {filter} -f rawvideo -pix_fmt {outputPixFmt} pipe:1";

            ProcessStartInfo info = new ProcessStartInfo
            {
                FileName = "ffmpeg",
                Arguments = args,
                RedirectStandardInput = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                UseShellExecute = false,
                CreateNoWindow = true
            };

            Process process = new Process
            {
                StartInfo = info
            };

            process.Start();
            _ = Task.Run(() => ReadFfmpegStderr(process));
            return process;
        }

        private static void ReadFfmpegStderr(Process process)
        {
            try
            {
                string? line;
                while ((line = process.StandardError.ReadLine()) != null)
                {
                    if (!string.IsNullOrWhiteSpace(line))
                    {
                        Console.WriteLine($"Video transform: {line}");
                    }
                }
            }
            catch
            {
                // Ignore stderr read failures.
            }
        }

        private static bool ReadExact(Stream stream, byte[] buffer, int length)
        {
            int offset = 0;
            while (offset < length)
            {
                int read = stream.Read(buffer, offset, length - offset);
                if (read <= 0)
                {
                    return false;
                }
                offset += read;
            }
            return true;
        }

        private void RestartPreview(CaptureFormat fmt)
        {
            StopPreview();
            StartPreviewIfEnabled(fmt);
        }

        private void StopPreview()
        {
            foreach (PreviewPipeline pipeline in _previewPipelines)
            {
                pipeline.Stop();
            }
            _previewPipelines.Clear();
        }

        private static string ResolveInputPixelFormat(int codec)
        {
            return codec switch
            {
                (int)OMTCodec.UYVY => "uyvy422",
                (int)OMTCodec.YUY2 => "yuyv422",
                (int)OMTCodec.NV12 => "nv12",
                _ => "yuyv422"
            };
        }

        private static bool TryGetFramebufferSize(string path, out int width, out int height)
        {
            width = 0;
            height = 0;
            try
            {
                string fb = Path.GetFileName(path);
                if (!fb.StartsWith("fb", StringComparison.OrdinalIgnoreCase))
                {
                    return false;
                }

                string sizePath = Path.Combine("/sys/class/graphics", fb, "virtual_size");
                if (!File.Exists(sizePath))
                {
                    return false;
                }

                string[] parts = File.ReadAllText(sizePath).Trim().Split(',');
                if (parts.Length != 2)
                {
                    return false;
                }

                if (!int.TryParse(parts[0], out width) || !int.TryParse(parts[1], out height))
                {
                    return false;
                }

                return width > 0 && height > 0;
            }
            catch
            {
                return false;
            }
        }
    }
}
