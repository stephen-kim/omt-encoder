using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using System.Linq;
using System.Collections.Concurrent;
using libomtnet;

namespace omtcapture
{
    internal sealed class AudioPipeline : IDisposable
    {
        private readonly SendCoordinator _coordinator;
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
        private int _hdmiChannels = 1;
        private int _trsChannels = 1;
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
        private float[] _lastMixBuffer = Array.Empty<float>();
        private bool _hasLastMix;
        private long _audioPtsBase;
        private long _audioSamplesSent;
        // Audio queue disabled (direct send) to avoid added latency/instability.
        private bool _running;
        private DateTime _lastLogTime = DateTime.MinValue;
        private DateTime _lastReadLogTime = DateTime.MinValue;
        private int _consecutiveReadFailures;
        private int _readFailuresTotal;
        private int _readFailuresHdmi;
        private int _readFailuresTrs;
        private DateTime _lastRestartAttempt = DateTime.MinValue;
        private static readonly double TimestampTo100Ns = 10_000_000.0 / Stopwatch.Frequency;
        private bool _expectHdmi;
        private bool _expectTrs;


        public AudioPipeline(SendCoordinator coordinator, AudioSettings settings)
        {
            _coordinator = coordinator;
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
                Name = "AudioPipeline",
                Priority = ThreadPriority.AboveNormal
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

                string mode = _settings.Mode.Trim().ToLowerInvariant();
                bool useHdmi = mode == "hdmi" || mode == "both";
                bool useTrs = mode == "trs" || mode == "both";

                if (useHdmi && string.IsNullOrWhiteSpace(_settings.HdmiDevice))
                {
                    Console.WriteLine("Audio pipeline: HDMI device not set; disabling HDMI input.");
                    useHdmi = false;
                }
                if (useTrs && string.IsNullOrWhiteSpace(_settings.TrsDevice))
                {
                    Console.WriteLine("Audio pipeline: TRS device not set; disabling TRS input.");
                    useTrs = false;
                }

                _expectHdmi = useHdmi;
                _expectTrs = useTrs;
                if (!TryStartInputs(useHdmi, useTrs, _settings.SampleRate, Math.Max(1, _settings.Channels), out int effectiveRate))
                {
                    Console.WriteLine("Audio pipeline error: No input devices could be started.");
                    return;
                }

                // FORCE STEREO OUTPUT
                int outputChannels = 2; // Always output stereo to OMT
                Console.WriteLine($"Audio pipeline started. Rate: {effectiveRate}, Output Channels: {outputChannels}, HDMI In: {_hdmiChannels}ch, TRS In: {_trsChannels}ch");

                int channels = outputChannels;

                /*
                if (false) // Force disabled to check if monitor is causing overruns/stuttering
                {
                    _monitorProcess = StartAPlayWithFallback(_settings.Monitor.Device, effectiveRate, channels, out _monitorFormat);
                    _monitorStream = _monitorProcess?.StandardInput.BaseStream;
                }
                */

                int samplesPerChannel = Math.Max(1, _settings.SamplesPerChannel);
                int outputSampleCount = outputChannels * samplesPerChannel; // Total samples in a stereo frame
                int outputByteCount = outputSampleCount * sizeof(float);
                int outputShortByteCount = outputSampleCount * sizeof(short);

                InitializeBuffers(samplesPerChannel, outputChannels, outputByteCount, outputShortByteCount);
                _lastMixBuffer = new float[outputSampleCount];
                _hasLastMix = false;
                _audioPtsBase = GetMonotonicTimestamp100ns();
                _audioSamplesSent = 0;

                while (_running && !_cts.IsCancellationRequested)
                {
                    bool read1 = _expectHdmi && TryReadAudio(_hdmiStream, _readBuffer1, _shortBuffer1, _tempBuffer1, _hdmiFormat);
                    bool read2 = _expectTrs && TryReadAudio(_trsStream, _readBuffer2, _shortBuffer2, _tempBuffer2, _trsFormat);

                    if (_expectHdmi && !read1)
                    {
                        _readFailuresHdmi++;
                    }
                    if (_expectTrs && !read2)
                    {
                        _readFailuresTrs++;
                    }
                    if ((_expectHdmi && !read1) || (_expectTrs && !read2))
                    {
                        _readFailuresTotal++;
                    }

                    if ((_expectHdmi && !read1) && (_expectTrs && !read2))
                    {
                        _consecutiveReadFailures++;
                        if (ShouldAttemptRestart())
                        {
                            if (TryRestartInputs(outputChannels, samplesPerChannel, ref effectiveRate, ref outputByteCount, ref outputShortByteCount))
                            {
                                _consecutiveReadFailures = 0;
                                _audioPtsBase = GetMonotonicTimestamp100ns();
                                _audioSamplesSent = 0;
                                continue;
                            }
                        }

                        // Conceal with last good frame; fall back to silence.
                        if (_hasLastMix)
                        {
                            Array.Copy(_lastMixBuffer, _mixBuffer, _mixBuffer.Length);
                        }
                        else
                        {
                            Array.Clear(_mixBuffer, 0, _mixBuffer.Length);
                        }
                        ConvertToPlanar(outputChannels, samplesPerChannel, outputSampleCount);
                        EnqueueAudioChunk(outputByteCount, effectiveRate, outputChannels, samplesPerChannel);
                        Thread.Sleep(5);
                        continue;
                    }

                    _consecutiveReadFailures = 0;
                    MixBuffersNew(read1, read2, samplesPerChannel);
                    Array.Copy(_mixBuffer, _lastMixBuffer, _mixBuffer.Length);
                    _hasLastMix = true;

                    if ((DateTime.Now - _lastLogTime).TotalSeconds >= 5)
                    {
                        LogAudioLevelsNew(read1, read2, samplesPerChannel);
                        _lastLogTime = DateTime.Now;
                    }

                    if ((DateTime.Now - _lastReadLogTime).TotalSeconds >= 5)
                    {
                        Console.WriteLine($"Audio read failures (last 5s): total={_readFailuresTotal}, hdmi={_readFailuresHdmi}, trs={_readFailuresTrs}");
                        _readFailuresTotal = 0;
                        _readFailuresHdmi = 0;
                        _readFailuresTrs = 0;
                        _lastReadLogTime = DateTime.Now;
                    }

                    // Monitor logic uses outputSampleCount (stereo)
                    if (_monitorStream != null && false) 
                    {
                        Array.Copy(_mixBuffer, _monitorBuffer, outputSampleCount);
                        ApplyGain(_monitorBuffer, _settings.Monitor.Gain);
                        try
                        {
                            if (_monitorFormat == AudioSampleFormat.S16)
                            {
                                for (int i = 0; i < outputSampleCount; i++)
                                {
                                    float sample = _monitorBuffer[i];
                                    sample = Math.Max(-1f, Math.Min(1f, sample));
                                    _monitorShortBuffer[i] = (short)(sample * 32767f);
                                }
                                Buffer.BlockCopy(_monitorShortBuffer, 0, _writeBuffer, 0, outputShortByteCount);
                                _monitorStream.Write(_writeBuffer, 0, outputShortByteCount);
                            }
                            else
                            {
                                Buffer.BlockCopy(_monitorBuffer, 0, _writeBuffer, 0, outputByteCount);
                                _monitorStream.Write(_writeBuffer, 0, outputByteCount);
                            }
                        }
                        catch (Exception ex)
                        {
                            Console.WriteLine($"Audio pipeline: Monitor failed (disabling): {ex.Message}");
                            _monitorStream.Close();
                            _monitorStream = null;
                        }
                    }

                    ConvertToPlanar(outputChannels, samplesPerChannel, outputSampleCount);
                    EnqueueAudioChunk(outputByteCount, effectiveRate, outputChannels, samplesPerChannel);
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

        private void InitializeBuffers(int samplesPerChannel, int outputChannels, int outputByteCount, int outputShortByteCount)
        {
            ReleaseBuffers();

            int outputSampleCount = outputChannels * samplesPerChannel;
            _mixBuffer = new float[outputSampleCount];
            _monitorBuffer = new float[outputSampleCount];

            // Temp buffers size depends on INPUT channels
            _tempBuffer1 = new float[_hdmiChannels * samplesPerChannel];
            _tempBuffer2 = new float[_trsChannels * samplesPerChannel];

            _shortBuffer1 = new short[_tempBuffer1.Length];
            _shortBuffer2 = new short[_tempBuffer2.Length];
            _monitorShortBuffer = new short[outputSampleCount];
            _planarBuffer = new float[outputSampleCount];

            // Read buffers need to be big enough for the largest input
            int maxInputBytes = Math.Max(_tempBuffer1.Length, _tempBuffer2.Length) * sizeof(float);
            _readBuffer1 = new byte[maxInputBytes];
            _readBuffer2 = new byte[maxInputBytes];
            _writeBuffer = new byte[Math.Max(outputByteCount, outputShortByteCount)];

            _planarHandle = GCHandle.Alloc(_planarBuffer, GCHandleType.Pinned);
        }

        private bool ShouldAttemptRestart()
        {
            if (_settings.RestartAfterFailedReads <= 0)
            {
                return false;
            }

            if (_consecutiveReadFailures < _settings.RestartAfterFailedReads)
            {
                return false;
            }

            int cooldownMs = Math.Max(0, _settings.RestartCooldownMs);
            return (DateTime.UtcNow - _lastRestartAttempt).TotalMilliseconds >= cooldownMs;
        }

        private bool TryRestartInputs(int outputChannels, int samplesPerChannel, ref int effectiveRate, ref int outputByteCount, ref int outputShortByteCount)
        {
            _lastRestartAttempt = DateTime.UtcNow;
            Console.WriteLine("Audio pipeline: no input data; attempting restart.");

            StopProcesses();
            string mode = _settings.Mode.Trim().ToLowerInvariant();
            bool useHdmi = mode == "hdmi" || mode == "both";
            bool useTrs = mode == "trs" || mode == "both";
            if (!TryStartInputs(useHdmi,
                useTrs,
                _settings.SampleRate,
                Math.Max(1, _settings.Channels),
                out int newRate))
            {
                return false;
            }

            effectiveRate = newRate;
            int outputSampleCount = outputChannels * samplesPerChannel;
            outputByteCount = outputSampleCount * sizeof(float);
            outputShortByteCount = outputSampleCount * sizeof(short);

            InitializeBuffers(samplesPerChannel, outputChannels, outputByteCount, outputShortByteCount);

            Console.WriteLine($"Audio pipeline: restart success. Rate: {effectiveRate}, HDMI In: {_hdmiChannels}ch, TRS In: {_trsChannels}ch");
            return true;
        }

        private void MixBuffersNew(bool hasFirst, bool hasSecond, int frames)
        {
            float mixGain = _settings.MixGain;
            // Always output stereo (2 channels)
            
            for (int i = 0; i < frames; i++)
            {
                float left = 0f;
                float right = 0f;

                // Mix HDMI
                if (hasFirst)
                {
                    if (_hdmiChannels == 1)
                    {
                        float val = _tempBuffer1[i]; // Mono
                        left += val;
                        right += val;
                    }
                    else // Stereo or more
                    {
                        left += _tempBuffer1[i * _hdmiChannels];
                        right += _tempBuffer1[i * _hdmiChannels + 1];
                    }
                }

                // Mix TRS
                if (hasSecond)
                {
                    if (_trsChannels == 1)
                    {
                        float val = _tempBuffer2[i]; // Mono
                        left += val;
                        right += val;
                    }
                    else
                    {
                        left += _tempBuffer2[i * _trsChannels];
                        right += _tempBuffer2[i * _trsChannels + 1];
                    }
                }

                // Apply Gain & Clamp
                left *= mixGain;
                right *= mixGain;
                
                left = Math.Clamp(left, -1f, 1f);
                right = Math.Clamp(right, -1f, 1f);

                _mixBuffer[i * 2] = left;
                _mixBuffer[i * 2 + 1] = right;
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
                // Removed Float32 priority. Trying S16_LE first to avoid static/crackling on some HDMI grabbers.
                Console.WriteLine($"Attempting audio start on {candidate} (S16_LE, {channels}ch, {sampleRate}Hz)");
                string argsS16 = $"-q -D {candidate} -B {_settings.ArecordBufferUsec} -F {_settings.ArecordPeriodUsec} -t raw -f S16_LE -c {channels} -r {sampleRate}";
                Process? s16Proc = StartProcess(resolved, argsS16, redirectInput: false, redirectOutput: true, label: "arecord", readStderr: false);
                if (s16Proc != null && !ProcessExitedWithError(s16Proc, "S16_LE", out failure))
                {
                    StartStderrReader(s16Proc, "arecord");
                    format = AudioSampleFormat.S16;
                    failure = AudioProcessFailure.None;
                    return s16Proc;
                }
                else
                {
                    Console.WriteLine($"Failed {candidate} S16_LE");
                }
                s16Proc?.Dispose();

                Console.WriteLine($"Attempting audio start on {candidate} (Float32, {channels}ch, {sampleRate}Hz)");
                string argsFloat = $"-q -D {candidate} -B {_settings.ArecordBufferUsec} -F {_settings.ArecordPeriodUsec} -t raw -f FLOAT_LE -c {channels} -r {sampleRate}";
                Process? floatProc = StartProcess(resolved, argsFloat, redirectInput: false, redirectOutput: true, label: "arecord", readStderr: false);
                if (floatProc != null && !ProcessExitedWithError(floatProc, "FLOAT_LE", out failure))
                {
                    StartStderrReader(floatProc, "arecord");
                    format = AudioSampleFormat.Float32;
                    failure = AudioProcessFailure.None;
                    return floatProc;
                }
                else
                {
                    Console.WriteLine($"Failed {candidate} Float32");
                }
                floatProc?.Dispose();
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

        private bool TryStartInputs(bool useHdmi, bool useTrs, int requestedRate, int requestedChannels, out int effectiveRate)
        {
            // We ignore requestedChannels for INPUTS and try to get what we can (1 or 2).
            // But we prefer 2.
            
            DeviceParams? hdmiParams = useHdmi ? TryReadHwParams(_settings.HdmiDevice) : null;
            DeviceParams? trsParams = useTrs ? TryReadHwParams(_settings.TrsDevice) : null;

            List<int> rateCandidates = BuildRateCandidates(requestedRate, hdmiParams, trsParams);
            // We iterate channels from 2 down to 1
            List<int> channelCandidates = new() { 2, 1 }; 

            foreach (int rate in rateCandidates)
            {
                StopProcesses();
                
                bool hdmiOk = false;
                if (useHdmi)
                {
                    // Try stereo first, then mono
                    foreach (int ch in channelCandidates)
                    {
                        if (StartInput(_settings.HdmiDevice, rate, ch, out _hdmiProcess, out _hdmiFormat))
                        {
                            _hdmiChannels = ch;
                            hdmiOk = true;
                            break;
                        }
                    }
                }
                else
                {
                    _hdmiChannels = 0;
                    hdmiOk = true;
                }

                bool trsOk = false;
                if (useTrs)
                {
                    foreach (int ch in channelCandidates)
                    {
                        if (StartInput(_settings.TrsDevice, rate, ch, out _trsProcess, out _trsFormat))
                        {
                            _trsChannels = ch;
                            trsOk = true;
                            break;
                        }
                    }
                }
                else
                {
                    _trsChannels = 0;
                    trsOk = true;
                }

                if (hdmiOk && trsOk)
                {
                    // Removed BufferedStream to fix latency/sync issues
                    _hdmiStream = _hdmiProcess?.StandardOutput.BaseStream;
                    _trsStream = _trsProcess?.StandardOutput.BaseStream;
                    effectiveRate = rate;
                    return true;
                }
            }
            
            // If we failed to start both, maybe we can start just one? (As per original logic fallback)
            // For now, let's keep it simple (original logic had fallback, I'll assume success or fail together for simplicity of this edit, 
            // OR stick to the existing robust fallback flow but decoupled)
            
            // Let's implement the fallback if one fails
            if (useHdmi && useTrs)
            {
                 foreach (int rate in rateCandidates)
                 {
                     StopProcesses();
                     // Try HDMI only
                     foreach (int ch in channelCandidates) {
                         if (StartInput(_settings.HdmiDevice, rate, ch, out _hdmiProcess, out _hdmiFormat)) {
                            _hdmiChannels = ch;
                            _hdmiStream = _hdmiProcess?.StandardOutput.BaseStream;
                            _trsStream = null;
                            _trsChannels = 0;
                            effectiveRate = rate;
                            Console.WriteLine("Audio pipeline: TRS input unavailable; using HDMI only.");
                            return true;
                        }
                    }
                     
                     StopProcesses();
                     // Try TRS only
                     foreach (int ch in channelCandidates) {
                         if (StartInput(_settings.TrsDevice, rate, ch, out _trsProcess, out _trsFormat)) {
                            _trsChannels = ch;
                            _trsStream = _trsProcess?.StandardOutput.BaseStream;
                            _hdmiStream = null;
                            _hdmiChannels = 0;
                            effectiveRate = rate;
                            Console.WriteLine("Audio pipeline: HDMI input unavailable; using TRS only.");
                            return true;
                        }
                    }
                 }
            }

            effectiveRate = requestedRate;
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
            // Always prefer requested channels (usually 2) then fallbacks.
            // We ignore HW intersection because 'arecord' via 'plughw' can adapt mono HW to stereo.
            List<int> candidates = new() { requestedChannels, 2, 1 };
            return candidates.Distinct().ToList();
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

        private static long GetMonotonicTimestamp100ns()
        {
            return (long)(Stopwatch.GetTimestamp() * TimestampTo100Ns);
        }

        private void EnqueueAudioChunk(int byteCount, int sampleRate, int channels, int samplesPerChannel)
        {
            byte[] payload = new byte[byteCount];
            Buffer.BlockCopy(_planarBuffer, 0, payload, 0, byteCount);

            long timestamp = _audioPtsBase + (long)(_audioSamplesSent * 10_000_000.0 / sampleRate);
            _audioSamplesSent += samplesPerChannel;
            _coordinator.EnqueueAudio(payload, sampleRate, channels, samplesPerChannel, timestamp);
        }

        private void LogAudioLevelsNew(bool hasHdmi, bool hasTrs, int frames)
        {
            double hdmiRms = -100;
            double trsRms = -100;
            double mixRms = -100;

            if (hasHdmi)
            {
                hdmiRms = CalculateRms(_tempBuffer1, frames * _hdmiChannels);
            }

            if (hasTrs)
            {
                trsRms = CalculateRms(_tempBuffer2, frames * _trsChannels);
            }

            mixRms = CalculateRms(_mixBuffer, frames * 2);

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
