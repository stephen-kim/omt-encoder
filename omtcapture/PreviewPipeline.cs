using System.Diagnostics;

namespace omtcapture
{
    internal sealed class PreviewPipeline : IDisposable
    {
        private readonly PreviewSettings _settings;
        private readonly VideoSettings _video;
        private Process? _process;

        public PreviewPipeline(PreviewSettings settings, VideoSettings video)
        {
            _settings = settings;
            _video = video;
        }

        public void Start()
        {
            if (!_settings.Enabled)
            {
                return;
            }

            try
            {
                string args = $"-loglevel error -f video4linux2 -video_size {_settings.Width}x{_settings.Height} -framerate {_settings.Fps} -i {_video.DevicePath} -vf format={_settings.PixelFormat} -f fbdev {_settings.OutputDevice}";
                _process = StartProcess("ffmpeg", args);
            }
            catch (Exception ex)
            {
                Console.WriteLine($"Preview pipeline error: {ex.Message}");
                _process = null;
            }
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
    }
}
