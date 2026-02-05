using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using libomtnet;

namespace omtcapture
{
    internal interface ISendQueue : IDisposable
    {
        void EnqueueAudio(byte[] payload, int sampleRate, int channels, int samplesPerChannel, long timestamp);
        bool EnqueueVideo(byte[] payload, OMTMediaFrame template, long timestamp);
    }

    internal sealed class SendCoordinator : ISendQueue
    {
        private sealed class SendItem : IDisposable
        {
            private GCHandle _handle;
            private bool _pinned;
            public OMTMediaFrame Frame;

            public SendItem(OMTMediaFrame frame, byte[] payload)
            {
                _handle = GCHandle.Alloc(payload, GCHandleType.Pinned);
                _pinned = true;
                frame.Data = _handle.AddrOfPinnedObject();
                frame.DataLength = payload.Length;
                Frame = frame;
            }

            public void Dispose()
            {
                if (_pinned)
                {
                    _handle.Free();
                    _pinned = false;
                }
            }
        }

        private readonly OMTSend _send;
        private readonly BlockingCollection<SendItem> _audioQueue;
        private readonly BlockingCollection<SendItem> _videoQueue;
        private readonly CancellationTokenSource _cts = new();
        private readonly Thread _thread;
        private readonly bool _forceZeroTimestamps;
        private int _audioDropped;
        private int _videoDropped;
        private int _audioSendZero;
        private int _videoSendZero;
        private DateTime _lastLog = DateTime.MinValue;

        private static readonly double TimestampTo100Ns = 10_000_000.0 / System.Diagnostics.Stopwatch.Frequency;

        public SendCoordinator(OMTSend send, SendSettings settings)
        {
            _send = send;
            int audioCapacity = Clamp(settings.AudioQueueCapacity, 1, 16);
            int videoCapacity = Clamp(settings.VideoQueueCapacity, 1, 8);
            _forceZeroTimestamps = settings.ForceZeroTimestamps;
            _audioQueue = new BlockingCollection<SendItem>(audioCapacity);
            _videoQueue = new BlockingCollection<SendItem>(videoCapacity);
            _thread = new Thread(SendLoop)
            {
                IsBackground = true,
                Name = "OMTSend",
                Priority = ThreadPriority.AboveNormal
            };
            _thread.Start();
        }

        public void EnqueueAudio(byte[] payload, int sampleRate, int channels, int samplesPerChannel, long timestamp)
        {
            OMTMediaFrame frame = new OMTMediaFrame
            {
                Type = OMTFrameType.Audio,
                Codec = (int)OMTCodec.FPA1,
                SampleRate = sampleRate,
                Channels = channels,
                SamplesPerChannel = samplesPerChannel,
                Timestamp = timestamp
            };

            SendItem item = new SendItem(frame, payload);
            if (!_audioQueue.TryAdd(item))
            {
                if (_audioQueue.TryTake(out SendItem? dropped))
                {
                    dropped.Dispose();
                    _audioDropped++;
                }
                if (!_audioQueue.TryAdd(item))
                {
                    item.Dispose();
                    _audioDropped++;
                }
            }
        }

        public bool EnqueueVideo(byte[] payload, OMTMediaFrame template, long timestamp)
        {
            template.Timestamp = timestamp;
            SendItem item = new SendItem(template, payload);
            if (!_videoQueue.TryAdd(item))
            {
                if (_videoQueue.TryTake(out SendItem? dropped))
                {
                    dropped.Dispose();
                    _videoDropped++;
                }
                if (!_videoQueue.TryAdd(item))
                {
                    item.Dispose();
                    _videoDropped++;
                    return false;
                }
            }
            return true;
        }

        private void SendLoop()
        {
            while (!_cts.IsCancellationRequested)
            {
                SendItem? item = null;
                try
                {
                    if (_audioQueue.TryTake(out item))
                    {
                        SendItemNow(item, isAudio: true);
                        item = null;
                        continue;
                    }

                    if (_videoQueue.TryTake(out item, 10, _cts.Token))
                    {
                        SendItemNow(item, isAudio: false);
                        item = null;
                    }
                }
                catch (OperationCanceledException)
                {
                    break;
                }
                finally
                {
                    item?.Dispose();
                }

                LogStats();
            }
        }

        private void SendItemNow(SendItem item, bool isAudio)
        {
            item.Frame.Timestamp = _forceZeroTimestamps ? 0 : GetMonotonicTimestamp100ns();
            int sent = _send.Send(item.Frame);
            if (sent == 0)
            {
                if (isAudio)
                {
                    _audioSendZero++;
                }
                else
                {
                    _videoSendZero++;
                }
            }

            item.Dispose();
            LogStats();
        }

        private static int Clamp(int value, int min, int max)
        {
            if (value < min) return min;
            if (value > max) return max;
            return value;
        }

        private static long GetMonotonicTimestamp100ns()
        {
            return (long)(System.Diagnostics.Stopwatch.GetTimestamp() * TimestampTo100Ns);
        }


        private void LogStats()
        {
            if ((DateTime.UtcNow - _lastLog).TotalSeconds < 5)
            {
                return;
            }

            if (_audioDropped > 0 || _videoDropped > 0 || _audioSendZero > 0 || _videoSendZero > 0)
            {
                Console.WriteLine($"Send stats (last 5s): audioDropped={_audioDropped}, videoDropped={_videoDropped}, audioSendZero={_audioSendZero}, videoSendZero={_videoSendZero}");
            }

            _audioDropped = 0;
            _videoDropped = 0;
            _audioSendZero = 0;
            _videoSendZero = 0;
            _lastLog = DateTime.UtcNow;
        }

        public void Dispose()
        {
            _cts.Cancel();
            _thread.Join(TimeSpan.FromSeconds(2));
            while (_audioQueue.TryTake(out SendItem? item))
            {
                item.Dispose();
            }
            while (_videoQueue.TryTake(out SendItem? item))
            {
                item.Dispose();
            }
            _cts.Dispose();
            _audioQueue.Dispose();
            _videoQueue.Dispose();
        }
    }
}
