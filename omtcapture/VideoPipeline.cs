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
        private volatile bool _restartRequested;

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

            capture.StartCapture();
            CaptureFrame captureFrame = new CaptureFrame();
            while (!token.IsCancellationRequested && !_restartRequested)
            {
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
                }
            }

            capture.StopCapture();
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
    }
}
