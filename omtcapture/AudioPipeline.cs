using System.Diagnostics;
using System.Runtime.InteropServices;
using libomtnet;

namespace omtcapture
{
    internal sealed class AudioPipeline : IDisposable
    {
        private readonly object _sendLock;
        private readonly OMTSend _send;
        private readonly AudioSettings _settings;
        private readonly CancellationTokenSource _cts = new();
        private Thread? _thread;
        private Process? _hdmiProcess;
        private Process? _trsProcess;
        private Process? _monitorProcess;
        private Stream? _hdmiStream;
        private Stream? _trsStream;
        private Stream? _monitorStream;
        private GCHandle _planarHandle;
        private float[] _planarBuffer = Array.Empty<float>();
        private float[] _mixBuffer = Array.Empty<float>();
        private float[] _monitorBuffer = Array.Empty<float>();
        private float[] _tempBuffer1 = Array.Empty<float>();
        private float[] _tempBuffer2 = Array.Empty<float>();
        private byte[] _readBuffer1 = Array.Empty<byte>();
        private byte[] _readBuffer2 = Array.Empty<byte>();
        private byte[] _writeBuffer = Array.Empty<byte>();
        private bool _running;

        public AudioPipeline(OMTSend send, object sendLock, AudioSettings settings)
        {
            _send = send;
            _sendLock = sendLock;
            _settings = settings;
        }

        public void Start()
        {
            string mode = _settings.Mode.Trim().ToLowerInvariant();
            if (mode == "none")
            {
                return;
            }
            if (mode != "hdmi" && mode != "trs" && mode != "both")
            {
                Console.WriteLine($"Audio mode '{_settings.Mode}' is not supported.");
                return;
            }

            _running = true;
            _thread = new Thread(Run)
            {
                IsBackground = true,
                Name = "AudioPipeline"
            };
            _thread.Start();
        }

        public void Stop()
        {
            _running = false;
            _cts.Cancel();
            _thread?.Join(TimeSpan.FromSeconds(2));
            _thread = null;
            StopProcesses();
            ReleaseBuffers();
        }

        public void Dispose()
        {
            Stop();
            _cts.Dispose();
        }

        private void Run()
        {
            try
            {
                int channels = Math.Max(1, _settings.Channels);
                int samplesPerChannel = Math.Max(1, _settings.SamplesPerChannel);
                int sampleCount = channels * samplesPerChannel;
                int byteCount = sampleCount * sizeof(float);

                _mixBuffer = new float[sampleCount];
                _monitorBuffer = new float[sampleCount];
                _tempBuffer1 = new float[sampleCount];
                _tempBuffer2 = new float[sampleCount];
                _planarBuffer = new float[sampleCount];
                _readBuffer1 = new byte[byteCount];
                _readBuffer2 = new byte[byteCount];
                _writeBuffer = new byte[byteCount];
                _planarHandle = GCHandle.Alloc(_planarBuffer, GCHandleType.Pinned);

                string mode = _settings.Mode.Trim().ToLowerInvariant();
                bool useHdmi = mode == "hdmi" || mode == "both";
                bool useTrs = mode == "trs" || mode == "both";

                if (useHdmi)
                {
                    _hdmiProcess = StartARecord(_settings.HdmiDevice, _settings.SampleRate, channels);
                    _hdmiStream = _hdmiProcess.StandardOutput.BaseStream;
                }

                if (useTrs)
                {
                    _trsProcess = StartARecord(_settings.TrsDevice, _settings.SampleRate, channels);
                    _trsStream = _trsProcess.StandardOutput.BaseStream;
                }

                if (_settings.Monitor.Enabled)
                {
                    _monitorProcess = StartAPlay(_settings.Monitor.Device, _settings.SampleRate, channels);
                    _monitorStream = _monitorProcess.StandardInput.BaseStream;
                }

                OMTMediaFrame audioFrame = new OMTMediaFrame
                {
                    Type = OMTFrameType.Audio,
                    Codec = (int)OMTCodec.FPA1,
                    SampleRate = _settings.SampleRate,
                    Channels = channels,
                    SamplesPerChannel = samplesPerChannel,
                    Data = _planarHandle.AddrOfPinnedObject(),
                    DataLength = byteCount,
                    Timestamp = -1
                };

                while (_running && !_cts.IsCancellationRequested)
                {
                    bool read1 = TryReadAudio(_hdmiStream, _readBuffer1, _tempBuffer1);
                    bool read2 = TryReadAudio(_trsStream, _readBuffer2, _tempBuffer2);

                    if (!read1 && !read2)
                    {
                        Thread.Sleep(5);
                        continue;
                    }

                    MixBuffers(read1, read2, sampleCount);

                    if (_monitorStream != null && _settings.Monitor.Enabled)
                    {
                        Array.Copy(_mixBuffer, _monitorBuffer, sampleCount);
                        ApplyGain(_monitorBuffer, _settings.Monitor.Gain);
                        Buffer.BlockCopy(_monitorBuffer, 0, _writeBuffer, 0, byteCount);
                        _monitorStream.Write(_writeBuffer, 0, _writeBuffer.Length);
                    }

                    ConvertToPlanar(channels, samplesPerChannel, sampleCount);

                    lock (_sendLock)
                    {
                        _send.Send(audioFrame);
                    }
                }
            }
            catch (Exception ex)
            {
                Console.WriteLine($"Audio pipeline error: {ex.Message}");
            }
            finally
            {
                StopProcesses();
            }
        }

        private void MixBuffers(bool hasFirst, bool hasSecond, int sampleCount)
        {
            float mixGain = _settings.MixGain;
            float scale = (hasFirst && hasSecond) ? mixGain : 1.0f;

            for (int i = 0; i < sampleCount; i++)
            {
                float sample = 0f;
                if (hasFirst)
                {
                    sample += _tempBuffer1[i];
                }
                if (hasSecond)
                {
                    sample += _tempBuffer2[i];
                }

                sample *= scale;
                if (sample > 1f)
                {
                    sample = 1f;
                }
                else if (sample < -1f)
                {
                    sample = -1f;
                }

                _mixBuffer[i] = sample;
            }
        }

        private void ApplyGain(float[] buffer, float gain)
        {
            if (Math.Abs(gain - 1.0f) < 0.0001f)
            {
                return;
            }

            for (int i = 0; i < buffer.Length; i++)
            {
                float sample = buffer[i] * gain;
                if (sample > 1f)
                {
                    sample = 1f;
                }
                else if (sample < -1f)
                {
                    sample = -1f;
                }

                buffer[i] = sample;
            }
        }

        private void ConvertToPlanar(int channels, int samplesPerChannel, int sampleCount)
        {
            for (int channel = 0; channel < channels; channel++)
            {
                int planarOffset = channel * samplesPerChannel;
                int interleavedOffset = channel;
                for (int sample = 0; sample < samplesPerChannel; sample++)
                {
                    _planarBuffer[planarOffset + sample] = _mixBuffer[interleavedOffset + (sample * channels)];
                }
            }
        }

        private bool TryReadAudio(Stream? stream, byte[] byteBuffer, float[] floatBuffer)
        {
            if (stream == null)
            {
                return false;
            }

            int bytesNeeded = byteBuffer.Length;
            int offset = 0;
            while (offset < bytesNeeded)
            {
                int read = stream.Read(byteBuffer, offset, bytesNeeded - offset);
                if (read <= 0)
                {
                    return false;
                }
                offset += read;
            }

            Buffer.BlockCopy(byteBuffer, 0, floatBuffer, 0, bytesNeeded);
            return true;
        }

        private Process StartARecord(string device, int sampleRate, int channels)
        {
            string args = $"-q -D {device} -f FLOAT_LE -c {channels} -r {sampleRate}";
            return StartProcess("arecord", args, redirectInput: false, redirectOutput: true);
        }

        private Process StartAPlay(string device, int sampleRate, int channels)
        {
            string args = $"-q -D {device} -f FLOAT_LE -c {channels} -r {sampleRate}";
            return StartProcess("aplay", args, redirectInput: true, redirectOutput: false);
        }

        private Process StartProcess(string fileName, string args, bool redirectInput, bool redirectOutput)
        {
            ProcessStartInfo info = new ProcessStartInfo
            {
                FileName = fileName,
                Arguments = args,
                RedirectStandardInput = redirectInput,
                RedirectStandardOutput = redirectOutput,
                RedirectStandardError = true,
                UseShellExecute = false,
                CreateNoWindow = true
            };

            Process process = new Process
            {
                StartInfo = info
            };

            process.Start();
            return process;
        }

        private void StopProcesses()
        {
            StopProcess(_hdmiProcess);
            StopProcess(_trsProcess);
            StopProcess(_monitorProcess);
            _hdmiProcess = null;
            _trsProcess = null;
            _monitorProcess = null;
            _hdmiStream = null;
            _trsStream = null;
            _monitorStream = null;
        }

        private void StopProcess(Process? process)
        {
            if (process == null)
            {
                return;
            }

            try
            {
                if (!process.HasExited)
                {
                    process.Kill(true);
                }
            }
            catch
            {
                // Ignore kill failures on shutdown.
            }

            process.Dispose();
        }

        private void ReleaseBuffers()
        {
            if (_planarHandle.IsAllocated)
            {
                _planarHandle.Free();
            }
            _planarBuffer = Array.Empty<float>();
            _mixBuffer = Array.Empty<float>();
            _monitorBuffer = Array.Empty<float>();
            _tempBuffer1 = Array.Empty<float>();
            _tempBuffer2 = Array.Empty<float>();
            _readBuffer1 = Array.Empty<byte>();
            _readBuffer2 = Array.Empty<byte>();
            _writeBuffer = Array.Empty<byte>();
        }
    }
}
