using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using System.Linq;
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
        private AudioSampleFormat _monitorFormat = AudioSampleFormat.Float32;
        private GCHandle _planarHandle;
        private float[] _planarBuffer = Array.Empty<float>();
        private float[] _mixBuffer = Array.Empty<float>();
        private float[] _outputBuffer = Array.Empty<float>();
        private float[] _monitorBuffer = Array.Empty<float>();
        private float[] _tempBuffer1 = Array.Empty<float>();
        private float[] _tempBuffer2 = Array.Empty<float>();
        private short[] _shortBuffer1 = Array.Empty<short>();
        private short[] _shortBuffer2 = Array.Empty<short>();
        private short[] _monitorShortBuffer = Array.Empty<short>();
        private byte[] _readBuffer1 = Array.Empty<byte>();
        private byte[] _readBuffer2 = Array.Empty<byte>();
        private byte[] _writeBuffer = Array.Empty<byte>();
        private byte[] _writeBuffer = Array.Empty<byte>();
        private bool _running;
        private DateTime _lastLogTime = DateTime.MinValue;


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

                string mode = _settings.Mode.Trim().ToLowerInvariant();
                bool useHdmi = mode == "hdmi" || mode == "both";
                bool useTrs = mode == "trs" || mode == "both";

                if (!TryStartInputs(useHdmi, useTrs, _settings.SampleRate, channels, out int effectiveRate, out int effectiveChannels))
                {
                    Console.WriteLine("Audio pipeline error: No input devices could be started.");
                    return;
                }

                Console.WriteLine($"Audio pipeline started. Rate: {effectiveRate}, Channels: {effectiveChannels}");

                channels = effectiveChannels;

                if (_settings.Monitor.Enabled)
                {
                    _monitorProcess = StartAPlayWithFallback(_settings.Monitor.Device, effectiveRate, channels, out _monitorFormat);
                    _monitorStream = _monitorProcess?.StandardInput.BaseStream;
                }

                int sampleCount = channels * samplesPerChannel;
                int byteCount = sampleCount * sizeof(float);
                int shortByteCount = sampleCount * sizeof(short);

                _mixBuffer = new float[sampleCount];
                _monitorBuffer = new float[sampleCount];
                _tempBuffer1 = new float[sampleCount];
                _tempBuffer2 = new float[sampleCount];
                _shortBuffer1 = new short[sampleCount];
                _shortBuffer2 = new short[sampleCount];
                _monitorShortBuffer = new short[sampleCount];
                _planarBuffer = new float[sampleCount];
                _readBuffer1 = new byte[Math.Max(byteCount, shortByteCount)];
                _readBuffer2 = new byte[Math.Max(byteCount, shortByteCount)];
                _writeBuffer = new byte[Math.Max(byteCount, shortByteCount)];
                _planarHandle = GCHandle.Alloc(_planarBuffer, GCHandleType.Pinned);

                OMTMediaFrame audioFrame = new OMTMediaFrame
                {
                    Type = OMTFrameType.Audio,
                    Codec = (int)OMTCodec.FPA1,
                    SampleRate = effectiveRate,
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

                    if ((DateTime.Now - _lastLogTime).TotalSeconds >= 5)
                    {
                        LogAudioLevels(read1, read2, sampleCount);
                        _lastLogTime = DateTime.Now;
                    }

                    if (_monitorStream != null && _settings.Monitor.Enabled)
                    {
                        Array.Copy(_mixBuffer, _monitorBuffer, sampleCount);
                        ApplyGain(_monitorBuffer, _settings.Monitor.Gain);
                        if (_monitorFormat == AudioSampleFormat.S16)
                        {
                            for (int i = 0; i < sampleCount; i++)
                            {
                                float sample = _monitorBuffer[i];
                                sample = Math.Max(-1f, Math.Min(1f, sample));
                                _monitorShortBuffer[i] = (short)(sample * 32767f);
                            }
                            Buffer.BlockCopy(_monitorShortBuffer, 0, _writeBuffer, 0, shortByteCount);
                            _monitorStream.Write(_writeBuffer, 0, shortByteCount);
                        }
                        else
                        {
                            Buffer.BlockCopy(_monitorBuffer, 0, _writeBuffer, 0, byteCount);
                            _monitorStream.Write(_writeBuffer, 0, byteCount);
                        }
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

        private Process? StartARecordWithFallback(string device, int sampleRate, int channels, out AudioSampleFormat format, out AudioProcessFailure failure)
        {
            failure = AudioProcessFailure.Other;
            string resolved = ResolveCommandPath("arecord");
            foreach (string candidate in BuildDeviceCandidates(device))
            {
                string argsFloat = $"-q -D {candidate} -f FLOAT_LE -c {channels} -r {sampleRate}";
                Process? floatProc = StartProcess(resolved, argsFloat, redirectInput: false, redirectOutput: true, label: "arecord", readStderr: false);
                if (floatProc != null && !ProcessExitedWithError(floatProc, "FLOAT_LE", out failure))
                {
                    StartStderrReader(floatProc, "arecord");
                    format = AudioSampleFormat.Float32;
                    failure = AudioProcessFailure.None;
                    return floatProc;
                }

                floatProc?.Dispose();
                string argsS16 = $"-q -D {candidate} -f S16_LE -c {channels} -r {sampleRate}";
                Process? s16Proc = StartProcess(resolved, argsS16, redirectInput: false, redirectOutput: true, label: "arecord", readStderr: false);
                if (s16Proc != null && !ProcessExitedWithError(s16Proc, "S16_LE", out failure))
                {
                    StartStderrReader(s16Proc, "arecord");
                    format = AudioSampleFormat.S16;
                    failure = AudioProcessFailure.None;
                    return s16Proc;
                }

                s16Proc?.Dispose();
            }

            format = AudioSampleFormat.Float32;
            if (failure == AudioProcessFailure.None)
            {
                failure = AudioProcessFailure.Other;
            }
            Console.WriteLine($"Audio pipeline error: Failed to start arecord for {device}.");
            return null;
        }

        private Process? StartAPlayWithFallback(string device, int sampleRate, int channels, out AudioSampleFormat format)
        {
            string resolved = ResolveCommandPath("aplay");
            foreach (string candidate in BuildDeviceCandidates(device))
            {
                string argsFloat = $"-q -D {candidate} -f FLOAT_LE -c {channels} -r {sampleRate}";
                Process? floatProc = StartProcess(resolved, argsFloat, redirectInput: true, redirectOutput: false, label: "aplay", readStderr: false);
                if (floatProc != null && !ProcessExitedWithError(floatProc, "FLOAT_LE", out _))
                {
                    StartStderrReader(floatProc, "aplay");
                    format = AudioSampleFormat.Float32;
                    return floatProc;
                }

                floatProc?.Dispose();
                string argsS16 = $"-q -D {candidate} -f S16_LE -c {channels} -r {sampleRate}";
                Process? s16Proc = StartProcess(resolved, argsS16, redirectInput: true, redirectOutput: false, label: "aplay", readStderr: true);
                if (s16Proc != null)
                {
                    format = AudioSampleFormat.S16;
                    return s16Proc;
                }

                s16Proc?.Dispose();
            }

            format = AudioSampleFormat.Float32;
            Console.WriteLine($"Audio pipeline error: Failed to start aplay for {device}.");
            return null;
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

        private static bool ProcessExitedWithError(Process process, string formatLabel, out AudioProcessFailure failure)
        {
            failure = AudioProcessFailure.None;
            try
            {
                if (process.WaitForExit(200))
                {
                    string error = process.StandardError.ReadToEnd().Trim();
                    Console.WriteLine($"arecord {formatLabel} failed: {error}");
                    if (error.Contains("channels count non available", StringComparison.OrdinalIgnoreCase))
                    {
                        failure = AudioProcessFailure.ChannelsUnsupported;
                    }
                    else if (!string.IsNullOrWhiteSpace(error))
                    {
                        failure = AudioProcessFailure.Other;
                    }
                    return true;
                }
            }
            catch
            {
                failure = AudioProcessFailure.Other;
                return true;
            }

            return false;
        }

        private static List<string> BuildDeviceCandidates(string device)
        {
            List<string> candidates = new() { device };
            if (device.StartsWith("hw:", StringComparison.OrdinalIgnoreCase))
            {
                string suffix = device.Substring(3);
                candidates.Add($"plughw:{suffix}");
                candidates.Add($"plug:hw:{suffix}");
            }
            return candidates.Distinct().ToList();
        }

        private bool TryStartInputs(bool useHdmi, bool useTrs, int requestedRate, int requestedChannels, out int effectiveRate, out int effectiveChannels)
        {
            DeviceParams? hdmiParams = useHdmi ? TryReadHwParams(_settings.HdmiDevice) : null;
            DeviceParams? trsParams = useTrs ? TryReadHwParams(_settings.TrsDevice) : null;

            List<int> rateCandidates = BuildRateCandidates(requestedRate, hdmiParams, trsParams);
            List<int> channelCandidates = BuildChannelCandidates(requestedChannels, hdmiParams, trsParams);

            foreach (int rate in rateCandidates)
            {
                foreach (int channels in channelCandidates)
                {
                    StopProcesses();
                    bool hdmiOk = !useHdmi || StartInput(_settings.HdmiDevice, rate, channels, out _hdmiProcess, out _hdmiFormat);
                    bool trsOk = !useTrs || StartInput(_settings.TrsDevice, rate, channels, out _trsProcess, out _trsFormat);

                    if (useHdmi && !hdmiOk)
                    {
                        continue;
                    }

                    if (useTrs && !trsOk)
                    {
                        continue;
                    }

                    _hdmiStream = _hdmiProcess?.StandardOutput.BaseStream;
                    _trsStream = _trsProcess?.StandardOutput.BaseStream;
                    effectiveRate = rate;
                    effectiveChannels = channels;
                    return useHdmi || useTrs;
                }
            }

            if (useHdmi && useTrs)
            {
                foreach (int rate in rateCandidates)
                {
                    foreach (int channels in channelCandidates)
                    {
                        StopProcesses();
                        if (StartInput(_settings.HdmiDevice, rate, channels, out _hdmiProcess, out _hdmiFormat))
                        {
                            Console.WriteLine("Audio pipeline: TRS input unavailable; using HDMI only.");
                            _hdmiStream = _hdmiProcess?.StandardOutput.BaseStream;
                            _trsStream = null;
                            effectiveRate = rate;
                            effectiveChannels = channels;
                            return true;
                        }

                        StopProcesses();
                        if (StartInput(_settings.TrsDevice, rate, channels, out _trsProcess, out _trsFormat))
                        {
                            Console.WriteLine("Audio pipeline: HDMI input unavailable; using TRS only.");
                            _trsStream = _trsProcess?.StandardOutput.BaseStream;
                            _hdmiStream = null;
                            effectiveRate = rate;
                            effectiveChannels = channels;
                            return true;
                        }
                    }
                }
            }

            effectiveRate = requestedRate;
            effectiveChannels = requestedChannels;
            return false;
        }

        private bool StartInput(string device, int sampleRate, int channels, out Process? process, out AudioSampleFormat format)
        {
            AudioProcessFailure failure;
            process = StartARecordWithFallback(device, sampleRate, channels, out format, out failure);
            if (process != null)
            {
                Console.WriteLine($"Started audio input on {device}. Rate: {sampleRate}, Channels: {channels}, Format: {format}");
                return true;
            }
            else
            {
                Console.WriteLine($"Failed to start audio input on {device}. Failure reason: {failure}");
                return false;
            }
        }

        private static DeviceParams? TryReadHwParams(string device)
        {
            string resolved = ResolveCommandPath("arecord");
            ProcessStartInfo info = new ProcessStartInfo
            {
                FileName = resolved,
                Arguments = $"-D {device} --dump-hw-params",
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                UseShellExecute = false,
                CreateNoWindow = true
            };

            try
            {
                using Process process = new Process { StartInfo = info };
                process.Start();
                string output = process.StandardOutput.ReadToEnd();
                string error = process.StandardError.ReadToEnd();
                process.WaitForExit(1000);

                string combined = string.IsNullOrWhiteSpace(output) ? error : output;
                if (string.IsNullOrWhiteSpace(combined))
                {
                    return null;
                }

                DeviceParams parameters = new DeviceParams();
                foreach (string line in combined.Split('\n'))
                {
                    string trimmed = line.Trim();
                    if (trimmed.StartsWith("FORMAT:", StringComparison.OrdinalIgnoreCase))
                    {
                        parameters.Formats = ParseTokens(trimmed);
                    }
                    else if (trimmed.StartsWith("CHANNELS:", StringComparison.OrdinalIgnoreCase))
                    {
                        parameters.Channels = ParseInts(trimmed);
                    }
                    else if (trimmed.StartsWith("RATE:", StringComparison.OrdinalIgnoreCase))
                    {
                        parameters.Rates = ParseInts(trimmed);
                    }
                }

                if (parameters.Formats.Count == 0 && parameters.Channels.Count == 0 && parameters.Rates.Count == 0)
                {
                    return null;
                }

                return parameters;
            }
            catch
            {
                return null;
            }
        }

        private static List<int> BuildRateCandidates(int requestedRate, DeviceParams? hdmiParams, DeviceParams? trsParams)
        {
            List<int> defaults = new() { requestedRate, 48000, 44100 };
            return BuildCandidateList(defaults, hdmiParams?.Rates, trsParams?.Rates);
        }

        private static List<int> BuildChannelCandidates(int requestedChannels, DeviceParams? hdmiParams, DeviceParams? trsParams)
        {
            List<int> defaults = new() { requestedChannels, 2, 1 };
            return BuildCandidateList(defaults, hdmiParams?.Channels, trsParams?.Channels);
        }

        private static List<int> BuildCandidateList(List<int> defaults, List<int>? hdmi, List<int>? trs)
        {
            IEnumerable<int> candidates = defaults;
            if (hdmi != null && hdmi.Count > 0 && trs != null && trs.Count > 0)
            {
                candidates = hdmi.Intersect(trs);
            }
            else if (hdmi != null && hdmi.Count > 0)
            {
                candidates = hdmi;
            }
            else if (trs != null && trs.Count > 0)
            {
                candidates = trs;
            }

            List<int> ordered = defaults.Concat(candidates).Distinct().ToList();
            return ordered;
        }

        private static List<int> ParseInts(string line)
        {
            List<int> values = new();
            int current = -1;
            foreach (char ch in line)
            {
                if (char.IsDigit(ch))
                {
                    int digit = ch - '0';
                    current = current < 0 ? digit : (current * 10 + digit);
                }
                else if (current >= 0)
                {
                    values.Add(current);
                    current = -1;
                }
            }
            if (current >= 0)
            {
                values.Add(current);
            }

            return values.Distinct().ToList();
        }

        private static List<string> ParseTokens(string line)
        {
            int idx = line.IndexOf(':');
            if (idx == -1)
            {
                return new List<string>();
            }
            string[] tokens = line[(idx + 1)..].Split(' ', StringSplitOptions.RemoveEmptyEntries);
            return tokens.ToList();
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

        private void LogAudioLevels(bool hasHdmi, bool hasTrs, int sampleCount)
        {
            double hdmiRms = -100;
            double trsRms = -100;
            double mixRms = -100;

            if (hasHdmi)
            {
                hdmiRms = CalculateRms(_tempBuffer1, sampleCount);
            }

            if (hasTrs)
            {
                trsRms = CalculateRms(_tempBuffer2, sampleCount);
            }

            mixRms = CalculateRms(_mixBuffer, sampleCount);

            Console.WriteLine($"Audio Levels (dB) -> HDMI: {hdmiRms:F1} | TRS: {trsRms:F1} | Mix: {mixRms:F1}");
        }

        private double CalculateRms(float[] buffer, int count)
        {
            double sum = 0;
            for (int i = 0; i < count; i++)
            {
                sum += buffer[i] * buffer[i];
            }
            double rms = Math.Sqrt(sum / count);
            return 20 * Math.Log10(rms + 1e-9); // 1e-9 to avoid log(0)
        }
    }

    internal enum AudioSampleFormat
    {
        Float32,
        S16
    }

    internal enum AudioProcessFailure
    {
        None,
        ChannelsUnsupported,
        Other
    }

    internal sealed class DeviceParams
    {
        public List<string> Formats { get; set; } = new();
        public List<int> Channels { get; set; } = new();
        public List<int> Rates { get; set; } = new();
    }
}
