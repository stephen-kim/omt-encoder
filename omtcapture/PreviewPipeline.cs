using System.Diagnostics;

namespace omtcapture
{
    internal sealed class PreviewPipeline : IDisposable
    {
        private readonly PreviewSettings _settings;
        private readonly string _inputPixelFormat;
        private readonly int _inputWidth;
        private readonly int _inputHeight;
        private Process? _process;
        private Stream? _stdin;
        private Task? _stderrTask;
        private byte[] _buffer = Array.Empty<byte>();
        private long _lastFrameTicks;
        private long _frameIntervalTicks;

        public PreviewPipeline(PreviewSettings settings, string inputPixelFormat, int inputWidth, int inputHeight)
        {
            _settings = settings;
            _inputPixelFormat = inputPixelFormat;
            _inputWidth = inputWidth;
            _inputHeight = inputHeight;
            _frameIntervalTicks = TimeSpan.FromSeconds(1.0 / Math.Max(1, _settings.Fps)).Ticks;
        }

        public void Start()
        {
            if (!_settings.Enabled)
            {
                return;
            }

            try
            {
                string filter = BuildFilter();
                string args = $"-loglevel error -f rawvideo -pix_fmt {_inputPixelFormat} -s {_inputWidth}x{_inputHeight} -r {_settings.Fps} -i pipe:0 -vf {filter} -f fbdev {_settings.OutputDevice}";
                _process = StartProcess("ffmpeg", args);
                _stdin = _process.StandardInput.BaseStream;
                _stderrTask = Task.Run(() => ReadStderr(_process));
            }
            catch (Exception ex)
            {
                Console.WriteLine($"Preview pipeline error: {ex.Message}");
                _process = null;
            }
        }

        public void SubmitFrame(IntPtr data, int length)
        {
            if (!_settings.Enabled || _stdin == null)
            {
                return;
            }

            long nowTicks = DateTime.UtcNow.Ticks;
            if (nowTicks - _lastFrameTicks < _frameIntervalTicks)
            {
                return;
            }
            _lastFrameTicks = nowTicks;

            if (_buffer.Length != length)
            {
                _buffer = new byte[length];
            }

            System.Runtime.InteropServices.Marshal.Copy(data, _buffer, 0, length);
            _stdin.Write(_buffer, 0, length);
        }

        public void Stop()
        {
            if (_process == null)
            {
                return;
            }

            try
            {
                if (!_process.HasExited)
                {
                    _process.Kill(true);
                }
            }
            catch
            {
                // Ignore shutdown errors.
            }

            _process.Dispose();
            _process = null;
            _stdin = null;
            _stderrTask = null;
        }

        public void Dispose()
        {
            Stop();
        }

        private static Process StartProcess(string fileName, string args)
        {
            ProcessStartInfo info = new ProcessStartInfo
            {
                FileName = fileName,
                Arguments = args,
                RedirectStandardInput = true,
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

        private static void ReadStderr(Process process)
        {
            try
            {
                string? line;
                while ((line = process.StandardError.ReadLine()) != null)
                {
                    if (!string.IsNullOrWhiteSpace(line))
                    {
                        Console.WriteLine($"Preview ffmpeg: {line}");
                    }
                }
            }
            catch
            {
                // Ignore stderr read failures.
            }
        }

        private string BuildFilter()
        {
            string filter = $"format={_settings.PixelFormat}";
            if (_settings.Width != _inputWidth || _settings.Height != _inputHeight)
            {
                filter = $"scale={_settings.Width}:{_settings.Height}:flags=fast_bilinear,{filter}";
            }
            return filter;
        }
    }
}
