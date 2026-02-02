using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
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
        private AudioSampleFormat _hdmiFormat = AudioSampleFormat.Float32;
        private AudioSampleFormat _trsFormat = AudioSampleFormat.Float32;
        private GCHandle _planarHandle;
        private float[] _planarBuffer = Array.Empty<float>();
        private float[] _mixBuffer = Array.Empty<float>();
        private float[] _monitorBuffer = Array.Empty<float>();
        private float[] _tempBuffer1 = Array.Empty<float>();
        private float[] _tempBuffer2 = Array.Empty<float>();
        private short[] _shortBuffer1 = Array.Empty<short>();
        private short[] _shortBuffer2 = Array.Empty<short>();
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
                int shortByteCount = sampleCount * sizeof(short);

                _mixBuffer = new float[sampleCount];
                _monitorBuffer = new float[sampleCount];
                _tempBuffer1 = new float[sampleCount];
                _tempBuffer2 = new float[sampleCount];
                _shortBuffer1 = new short[sampleCount];
                _shortBuffer2 = new short[sampleCount];
                _planarBuffer = new float[sampleCount];
                _readBuffer1 = new byte[Math.Max(byteCount, shortByteCount)];
                _readBuffer2 = new byte[Math.Max(byteCount, shortByteCount)];
                _writeBuffer = new byte[byteCount];
                _planarHandle = GCHandle.Alloc(_planarBuffer, GCHandleType.Pinned);

                string mode = _settings.Mode.Trim().ToLowerInvariant();
                bool useHdmi = mode == "hdmi" || mode == "both";
                bool useTrs = mode == "trs" || mode == "both";

                if (useHdmi)
                {
                    _hdmiProcess = StartARecordWithFallback(_settings.HdmiDevice, _settings.SampleRate, channels, out _hdmiFormat);
                    _hdmiStream = _hdmiProcess?.StandardOutput.BaseStream;
                }

                if (useTrs)
                {
                    _trsProcess = StartARecordWithFallback(_settings.TrsDevice, _settings.SampleRate, channels, out _trsFormat);
                    _trsStream = _trsProcess?.StandardOutput.BaseStream;
                }

                if (_settings.Monitor.Enabled)
                {
                    _monitorProcess = StartAPlay(_settings.Monitor.Device, _settings.SampleRate, channels);
                    _monitorStream = _monitorProcess?.StandardInput.BaseStream;
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
                    bool read1 = TryReadAudio(_hdmiStream, _readBuffer1, _shortBuffer1, _tempBuffer1, _hdmiFormat);
                    bool read2 = TryReadAudio(_trsStream, _readBuffer2, _shortBuffer2, _tempBuffer2, _trsFormat);

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

        private bool TryReadAudio(Stream? stream, byte[] byteBuffer, short[] shortBuffer, float[] floatBuffer, AudioSampleFormat format)
        {
            if (stream == null)
            {
                return false;
            }

            int bytesNeeded = format == AudioSampleFormat.S16
                ? shortBuffer.Length * sizeof(short)
                : floatBuffer.Length * sizeof(float);

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

            if (format == AudioSampleFormat.S16)
            {
                Buffer.BlockCopy(byteBuffer, 0, shortBuffer, 0, bytesNeeded);
                for (int i = 0; i < floatBuffer.Length; i++)
                {
                    floatBuffer[i] = shortBuffer[i] / 32768f;
                }
            }
            else
            {
                Buffer.BlockCopy(byteBuffer, 0, floatBuffer, 0, bytesNeeded);
            }

            return true;
        }

        private Process? StartARecordWithFallback(string device, int sampleRate, int channels, out AudioSampleFormat format)
        {
            string resolved = ResolveCommandPath("arecord");
            string argsFloat = $"-q -D {device} -f FLOAT_LE -c {channels} -r {sampleRate}";
            Process? floatProc = StartProcess(resolved, argsFloat, redirectInput: false, redirectOutput: true, label: "arecord", readStderr: false);
            if (floatProc != null && !ProcessExitedWithError(floatProc, "FLOAT_LE"))
            {
                StartStderrReader(floatProc, "arecord");
                format = AudioSampleFormat.Float32;
                return floatProc;
            }

            floatProc?.Dispose();
            string argsS16 = $"-q -D {device} -f S16_LE -c {channels} -r {sampleRate}";
            Process? s16Proc = StartProcess(resolved, argsS16, redirectInput: false, redirectOutput: true, label: "arecord", readStderr: false);
            if (s16Proc != null && !ProcessExitedWithError(s16Proc, "S16_LE"))
            {
                StartStderrReader(s16Proc, "arecord");
                format = AudioSampleFormat.S16;
                return s16Proc;
            }

            s16Proc?.Dispose();
            format = AudioSampleFormat.Float32;
            Console.WriteLine($"Audio pipeline error: Failed to start arecord for {device}.");
            return null;
        }

        private Process? StartAPlay(string device, int sampleRate, int channels)
        {
            string args = $"-q -D {device} -f FLOAT_LE -c {channels} -r {sampleRate}";
            string resolved = ResolveCommandPath("aplay");
            return StartProcess(resolved, args, redirectInput: true, redirectOutput: false, label: "aplay", readStderr: true);
        }

        private Process? StartProcess(string fileName, string args, bool redirectInput, bool redirectOutput, string label, bool readStderr)
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

            try
            {
                process.Start();
                if (readStderr)
                {
                    _ = Task.Run(() => ReadStderr(process, label));
                }
                return process;
            }
            catch (Exception ex)
            {
                Console.WriteLine($"Audio pipeline error: Failed to start {label}: {ex.Message}");
                return null;
            }
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

        private static string ResolveCommandPath(string fileName)
        {
            if (fileName.Contains('/'))
            {
                return fileName;
            }

            if (File.Exists(fileName))
            {
                return fileName;
            }

            string[] candidates =
            {
                Path.Combine("/usr/bin", fileName),
                Path.Combine("/bin", fileName)
            };

            foreach (string candidate in candidates)
            {
                if (File.Exists(candidate))
                {
                    return candidate;
                }
            }

            return fileName;
        }

        private static void ReadStderr(Process process, string label)
        {
            try
            {
                string? line;
                while ((line = process.StandardError.ReadLine()) != null)
                {
                    if (!string.IsNullOrWhiteSpace(line))
                    {
                        Console.WriteLine($"{label}: {line}");
                    }
                }
            }
            catch
            {
                // Ignore stderr read failures.
            }
        }

        private static void StartStderrReader(Process process, string label)
        {
            _ = Task.Run(() => ReadStderr(process, label));
        }

        private static bool ProcessExitedWithError(Process process, string formatLabel)
        {
            try
            {
                if (process.WaitForExit(200))
                {
                    string error = process.StandardError.ReadToEnd().Trim();
                    Console.WriteLine($"arecord {formatLabel} failed: {error}");
                    return true;
                }
            }
            catch
            {
                return true;
            }

            return false;
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
            _shortBuffer1 = Array.Empty<short>();
            _shortBuffer2 = Array.Empty<short>();
            _readBuffer1 = Array.Empty<byte>();
            _readBuffer2 = Array.Empty<byte>();
            _writeBuffer = Array.Empty<byte>();
        }
    }

    internal enum AudioSampleFormat
    {
        Float32,
        S16
    }
}
