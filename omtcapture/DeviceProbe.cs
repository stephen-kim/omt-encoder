using System.Diagnostics;
using System.IO;

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
                Framebuffers = ListDeviceNodes("/dev", "fb*"),
                DisplayMode = GetDisplayMode()
            };
        }

        private static string RunCommand(string fileName, string args)
        {
            try
            {
                string resolved = ResolveCommandPath(fileName);
                ProcessStartInfo info = new ProcessStartInfo
                {
                    FileName = resolved,
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

        private static string GetDisplayMode()
        {
            string lightdm = RunCommand("systemctl", "is-active lightdm").Trim();
            if (lightdm == "active")
            {
                return "desktop";
            }

            string gdm = RunCommand("systemctl", "is-active gdm").Trim();
            if (gdm == "active")
            {
                return "desktop";
            }

            string defaultTarget = RunCommand("systemctl", "get-default").Trim();
            if (defaultTarget.Contains("graphical"))
            {
                return "desktop";
            }

            return "console";
        }
    }

    internal sealed class DeviceSnapshot
    {
        public string AudioInputs { get; set; } = string.Empty;
        public string AudioOutputs { get; set; } = string.Empty;
        public List<string> VideoDevices { get; set; } = new();
        public List<string> Framebuffers { get; set; } = new();
        public string DisplayMode { get; set; } = "unknown";
    }
}
