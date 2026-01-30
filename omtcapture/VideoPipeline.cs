using System.IO;
using libomtnet;
using omtcapture.capture;
using V4L2;

namespace omtcapture
{
    internal sealed class VideoPipeline : IDisposable
    {
        private readonly OMTSend _send;
        private readonly object _sendLock;
        private readonly object _settingsLock = new();
        private CancellationTokenSource? _cts;
        private Thread? _thread;
        private VideoSettings _settings;
        private PreviewSettings _previewSettings = new();
        private readonly List<PreviewPipeline> _previewPipelines = new();
        private readonly object _previewLock = new();
        private volatile bool _restartRequested;
        private volatile bool _previewRestartRequested;

        public VideoPipeline(OMTSend send, object sendLock, VideoSettings settings)
        {
            _send = send;
            _sendLock = sendLock;
            _settings = Clone(settings);
        }

        public void Start()
        {
            _cts = new CancellationTokenSource();
            _thread = new Thread(() => Run(_cts.Token))
            {
                IsBackground = true,
                Name = "VideoPipeline"
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

            switch (settings.Codec)
            {
                case "UYVY":
                    frame.Codec = (int)OMTCodec.UYVY;
                    frame.Stride = settings.Width * 2;
                    break;
                case "YUY2":
                    frame.Codec = (int)OMTCodec.YUY2;
                    frame.Stride = settings.Width * 2;
                    break;
                case "NV12":
                    frame.Codec = (int)OMTCodec.NV12;
                    frame.Stride = settings.Width;
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

            CaptureFormat fmt = new CaptureFormat(frame.Codec, frame.Width, frame.Height, frame.Stride, frame.FrameRateN, frame.FrameRateD);

            int frameCount = 0;
            long sentLength = 0;

            using CaptureDevice capture = new V4L2Capture(settings.DevicePath, fmt);

            fmt = capture.Format;
            if (fmt.Codec == (int)V4L2Unmanaged.PIXEL_FORMAT_YUYV)
            {
                fmt.Codec = (int)OMTCodec.YUY2;
            }

            frame.Codec = fmt.Codec;
            frame.Width = fmt.Width;
            frame.Height = fmt.Height;
            frame.Stride = fmt.Stride;
            frame.FrameRateD = fmt.FrameRateD;
            frame.FrameRateN = fmt.FrameRateN;

            Console.WriteLine("Format: " + fmt.ToString());

            StartPreviewIfEnabled(fmt);

            capture.StartCapture();
            CaptureFrame captureFrame = new CaptureFrame();
            while (!token.IsCancellationRequested && !_restartRequested)
            {
                if (_previewRestartRequested)
                {
                    RestartPreview(fmt);
                    _previewRestartRequested = false;
                }

                if (capture.GetNextFrame(ref captureFrame))
                {
                    frame.Data = captureFrame.Data;
                    frame.DataLength = captureFrame.Length;
                    frame.Timestamp = captureFrame.Timestamp;

                    int networkSend;
                    lock (_sendLock)
                    {
                        networkSend = _send.Send(frame);
                    }

                    frameCount += 1;
                    sentLength += networkSend;
                    if (frameCount >= 60)
                    {
                        Console.WriteLine("Sent " + frameCount + " frames, " + sentLength + " bytes.");
                        frameCount = 0;
                        sentLength = 0;
                    }

                    foreach (PreviewPipeline pipeline in _previewPipelines)
                    {
                        pipeline.SubmitFrame(captureFrame.Data, captureFrame.Length);
                    }
                }
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
