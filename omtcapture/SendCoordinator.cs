using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.InteropServices;
using libomtnet;

namespace omtcapture
{
    internal sealed class SendCoordinator : IDisposable
    {
        private readonly OMTSend _send;
        private readonly ConcurrentQueue<AudioChunk> _audioQueue = new();
        private readonly object _videoLock = new();
        private VideoChunk? _latestVideo;
        private readonly AutoResetEvent _signal = new(false);
        private Thread? _thread;
        private volatile bool _running;
        private int _audioQueueCount;
        private long _lastVideoSendTicks;
        private const int MaxAudioQueue = 8;

        public SendCoordinator(OMTSend send)
        {
            _send = send;
        }

        public void Start()
        {
            _running = true;
            _thread = new Thread(Run)
            {
                IsBackground = true,
                Name = "SendCoordinator",
                Priority = ThreadPriority.AboveNormal
            };
            _thread.Start();
        }

        public void Dispose()
        {
            _running = false;
            _signal.Set();
            _thread?.Join(TimeSpan.FromSeconds(2));
            _thread = null;
            DrainQueues();
            _signal.Dispose();
        }

        public void EnqueueAudio(byte[] data, int sampleRate, int channels, int samplesPerChannel, long timestamp)
        {
            AudioChunk chunk = new AudioChunk
            {
                Data = data,
                SampleRate = sampleRate,
                Channels = channels,
                SamplesPerChannel = samplesPerChannel,
                Timestamp = timestamp
            };

            _audioQueue.Enqueue(chunk);
            int count = Interlocked.Increment(ref _audioQueueCount);
            while (count > MaxAudioQueue && _audioQueue.TryDequeue(out _))
            {
                count = Interlocked.Decrement(ref _audioQueueCount);
            }
            _signal.Set();
        }

        public void EnqueueVideo(byte[] data, int width, int height, int stride, int codec, int frameRateN, int frameRateD, long timestamp)
        {
            VideoChunk chunk = new VideoChunk
            {
                Data = data,
                Width = width,
                Height = height,
                Stride = stride,
                Codec = codec,
                FrameRateN = frameRateN,
                FrameRateD = frameRateD,
                Timestamp = timestamp
            };

            lock (_videoLock)
            {
                if (_latestVideo != null)
                {
                    _latestVideo.Release();
                }
                _latestVideo = chunk;
            }

            _signal.Set();
        }

        private void Run()
        {
            while (_running)
            {
                _signal.WaitOne(5);

                int audioBurst = 0;
                while (_audioQueue.TryDequeue(out AudioChunk? audio))
                {
                    Interlocked.Decrement(ref _audioQueueCount);
                    SendAudio(audio);
                    audioBurst++;

                    if (audioBurst >= 2)
                    {
                        audioBurst = 0;
                        TrySendVideoIfDue();
                    }
                }

                TrySendVideoIfDue();
            }
        }

        private void TrySendVideoIfDue()
        {
            VideoChunk? chunk;
            lock (_videoLock)
            {
                chunk = _latestVideo;
                _latestVideo = null;
            }

            if (chunk == null)
            {
                return;
            }

            double fps = chunk.FrameRateD == 0 ? 30.0 : (double)chunk.FrameRateN / chunk.FrameRateD;
            double minIntervalTicks = Stopwatch.Frequency / Math.Max(1.0, fps);
            long nowTicks = Stopwatch.GetTimestamp();
            if (_lastVideoSendTicks > 0 && (nowTicks - _lastVideoSendTicks) < minIntervalTicks)
            {
                lock (_videoLock)
                {
                    if (_latestVideo == null)
                    {
                        _latestVideo = chunk;
                        return;
                    }
                }
                chunk.Release();
                return;
            }

            SendVideo(chunk);
            _lastVideoSendTicks = nowTicks;
        }

        private void SendAudio(AudioChunk chunk)
        {
            GCHandle handle = GCHandle.Alloc(chunk.Data, GCHandleType.Pinned);
            try
            {
                OMTMediaFrame frame = new OMTMediaFrame
                {
                    Type = OMTFrameType.Audio,
                    Codec = (int)OMTCodec.FPA1,
                    SampleRate = chunk.SampleRate,
                    Channels = chunk.Channels,
                    SamplesPerChannel = chunk.SamplesPerChannel,
                    Data = handle.AddrOfPinnedObject(),
                    DataLength = chunk.Data.Length,
                    Timestamp = chunk.Timestamp
                };
                _send.Send(frame);
            }
            finally
            {
                handle.Free();
            }
        }

        private void SendVideo(VideoChunk chunk)
        {
            GCHandle handle = GCHandle.Alloc(chunk.Data, GCHandleType.Pinned);
            try
            {
                OMTMediaFrame frame = new OMTMediaFrame
                {
                    Type = OMTFrameType.Video,
                    Codec = chunk.Codec,
                    Width = chunk.Width,
                    Height = chunk.Height,
                    Stride = chunk.Stride,
                    FrameRateN = chunk.FrameRateN,
                    FrameRateD = chunk.FrameRateD,
                    ColorSpace = OMTColorSpace.BT709,
                    Data = handle.AddrOfPinnedObject(),
                    DataLength = chunk.Data.Length,
                    Timestamp = chunk.Timestamp
                };
                _send.Send(frame);
            }
            finally
            {
                handle.Free();
                chunk.Release();
            }
        }

        private void DrainQueues()
        {
            while (_audioQueue.TryDequeue(out _))
            {
                Interlocked.Decrement(ref _audioQueueCount);
            }
            lock (_videoLock)
            {
                _latestVideo?.Release();
                _latestVideo = null;
            }
        }

        private sealed class AudioChunk
        {
            public byte[] Data { get; set; } = Array.Empty<byte>();
            public int SampleRate { get; set; }
            public int Channels { get; set; }
            public int SamplesPerChannel { get; set; }
            public long Timestamp { get; set; }
        }

        private sealed class VideoChunk
        {
            public byte[] Data { get; set; } = Array.Empty<byte>();
            public int Width { get; set; }
            public int Height { get; set; }
            public int Stride { get; set; }
            public int Codec { get; set; }
            public int FrameRateN { get; set; }
            public int FrameRateD { get; set; }
            public long Timestamp { get; set; }

            public void Release()
            {
                // Let GC reclaim the buffer; could be replaced with ArrayPool if needed.
            }
        }
    }
}
