using System.Diagnostics;

namespace omtcapture
{
    internal static class DeviceProbe
    {
        public static DeviceSnapshot GetSnapshot()
        {
            return new DeviceSnapshot
            {
                AudioInputs = RunCommand("arecord", "-l"),
                AudioOutputs = RunCommand("aplay", "-l"),
                VideoDevices = ListDeviceNodes("/dev", "video*"),
                Framebuffers = ListDeviceNodes("/dev", "fb*")
            };
        }

        private static string RunCommand(string fileName, string args)
        {
            try
            {
                ProcessStartInfo info = new ProcessStartInfo
                {
                    FileName = fileName,
                    Arguments = args,
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                    UseShellExecute = false,
                    CreateNoWindow = true
                };

                using Process process = new Process
                {
                    StartInfo = info
                };

                process.Start();
                string output = process.StandardOutput.ReadToEnd();
                string error = process.StandardError.ReadToEnd();
                process.WaitForExit(2000);

                if (!string.IsNullOrWhiteSpace(error))
                {
                    return output + Environment.NewLine + error;
                }

                return output;
            }
            catch (Exception ex)
            {
                return $"{fileName} failed: {ex.Message}";
            }
        }

        private static List<string> ListDeviceNodes(string directory, string pattern)
        {
            try
            {
                return Directory.GetFiles(directory, pattern).OrderBy(path => path).ToList();
            }
            catch
            {
                return new List<string>();
            }
        }
    }

    internal sealed class DeviceSnapshot
    {
        public string AudioInputs { get; set; } = string.Empty;
        public string AudioOutputs { get; set; } = string.Empty;
        public List<string> VideoDevices { get; set; } = new();
        public List<string> Framebuffers { get; set; } = new();
    }
}
