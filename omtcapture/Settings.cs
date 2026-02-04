using System.Xml;

namespace omtcapture
{
    internal sealed class Settings
    {
        public VideoSettings Video { get; private set; } = new();
        public AudioSettings Audio { get; private set; } = new();
        public PreviewSettings Preview { get; private set; } = new();
        public WebSettings Web { get; private set; } = new();

        public static Settings Load(string path)
        {
            Settings settings = new();
            XmlDocument config = new XmlDocument();
            config.Load(path);

            XmlNode? root = config.SelectSingleNode("settings");
            if (root == null)
            {
                throw new InvalidOperationException("config.xml missing settings element");
            }

            settings.Video = VideoSettings.Load(root);
            settings.Audio = AudioSettings.Load(root.SelectSingleNode("audio"));
            settings.Preview = PreviewSettings.Load(root.SelectSingleNode("preview"), settings.Video);
            settings.Web = WebSettings.Load(root.SelectSingleNode("web"));

            return settings;
        }

        public void Save(string path)
        {
            XmlDocument doc = new XmlDocument();
            XmlDeclaration declaration = doc.CreateXmlDeclaration("1.0", "utf-8", null);
            doc.AppendChild(declaration);

            XmlElement root = doc.CreateElement("settings");
            doc.AppendChild(root);

            Video.Save(doc, root);
            Audio.Save(doc, root);
            Preview.Save(doc, root);
            Web.Save(doc, root);

            doc.Save(path);
        }
    }

    internal sealed class VideoSettings
    {
        public string Name { get; set; } = "Video";
        public string DevicePath { get; set; } = "/dev/video0";
        public int Width { get; set; } = 1920;
        public int Height { get; set; } = 1080;
        public int FrameRateN { get; set; } = 60000;
        public int FrameRateD { get; set; } = 1001;
        public string Codec { get; set; } = "YUY2";

        public static VideoSettings Load(XmlNode root)
        {
            VideoSettings settings = new();
            settings.Name = ReadString(root, "name", settings.Name);
            settings.DevicePath = ReadString(root, "devicePath", settings.DevicePath);
            settings.Width = ReadInt(root, "width", settings.Width);
            settings.Height = ReadInt(root, "height", settings.Height);
            settings.FrameRateN = ReadInt(root, "frameRateN", settings.FrameRateN);
            settings.FrameRateD = ReadInt(root, "frameRateD", settings.FrameRateD);
            settings.Codec = ReadString(root, "codec", settings.Codec);
            return settings;
        }

        public void Save(XmlDocument doc, XmlElement root)
        {
            AppendChild(doc, root, "name", Name);
            AppendChild(doc, root, "devicePath", DevicePath);
            AppendChild(doc, root, "width", Width.ToString());
            AppendChild(doc, root, "height", Height.ToString());
            AppendChild(doc, root, "frameRateN", FrameRateN.ToString());
            AppendChild(doc, root, "frameRateD", FrameRateD.ToString());
            AppendChild(doc, root, "codec", Codec);
        }

        private static string ReadString(XmlNode root, string name, string fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            return node?.InnerText ?? fallback;
        }

        private static int ReadInt(XmlNode root, string name, int fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !int.TryParse(node.InnerText, out int value))
            {
                return fallback;
            }

            return value;
        }

        private static void AppendChild(XmlDocument doc, XmlElement root, string name, string value)
        {
            XmlElement child = doc.CreateElement(name);
            child.InnerText = value;
            root.AppendChild(child);
        }
    }

    internal sealed class AudioSettings
    {
        public string Mode { get; set; } = "both"; // none|hdmi|trs|both
        public string HdmiDevice { get; set; } = "hw:3,0";
        public string TrsDevice { get; set; } = "hw:2,0";
        public int SampleRate { get; set; } = 48000;
        public int Channels { get; set; } = 2;
        public int SamplesPerChannel { get; set; } = 480;
        public float MixGain { get; set; } = 0.5f;
        public int ArecordBufferUsec { get; set; } = 200000;
        public int ArecordPeriodUsec { get; set; } = 50000;
        public int RestartAfterFailedReads { get; set; } = 5;
        public int RestartCooldownMs { get; set; } = 1000;
        public MonitorSettings Monitor { get; set; } = new();

        public static AudioSettings Load(XmlNode? root)
        {
            AudioSettings settings = new();
            if (root == null)
            {
                return settings;
            }

            settings.Mode = ReadString(root, "mode", settings.Mode);
            settings.HdmiDevice = ReadString(root, "hdmiDevice", settings.HdmiDevice);
            settings.TrsDevice = ReadString(root, "trsDevice", settings.TrsDevice);
            settings.SampleRate = ReadInt(root, "sampleRate", settings.SampleRate);
            settings.Channels = ReadInt(root, "channels", settings.Channels);
            settings.SamplesPerChannel = ReadInt(root, "samplesPerChannel", settings.SamplesPerChannel);
            settings.MixGain = ReadFloat(root, "mixGain", settings.MixGain);
            settings.ArecordBufferUsec = ReadInt(root, "arecordBufferUsec", settings.ArecordBufferUsec);
            settings.ArecordPeriodUsec = ReadInt(root, "arecordPeriodUsec", settings.ArecordPeriodUsec);
            settings.RestartAfterFailedReads = ReadInt(root, "restartAfterFailedReads", settings.RestartAfterFailedReads);
            settings.RestartCooldownMs = ReadInt(root, "restartCooldownMs", settings.RestartCooldownMs);

            XmlNode? monitorNode = root.SelectSingleNode("monitor");
            settings.Monitor = MonitorSettings.Load(monitorNode);

            return settings;
        }

        public void Save(XmlDocument doc, XmlElement root)
        {
            XmlElement audio = doc.CreateElement("audio");
            root.AppendChild(audio);

            AppendChild(doc, audio, "mode", Mode);
            AppendChild(doc, audio, "hdmiDevice", HdmiDevice);
            AppendChild(doc, audio, "trsDevice", TrsDevice);
            AppendChild(doc, audio, "sampleRate", SampleRate.ToString());
            AppendChild(doc, audio, "channels", Channels.ToString());
            AppendChild(doc, audio, "samplesPerChannel", SamplesPerChannel.ToString());
            AppendChild(doc, audio, "mixGain", MixGain.ToString("0.###", System.Globalization.CultureInfo.InvariantCulture));
            AppendChild(doc, audio, "arecordBufferUsec", ArecordBufferUsec.ToString());
            AppendChild(doc, audio, "arecordPeriodUsec", ArecordPeriodUsec.ToString());
            AppendChild(doc, audio, "restartAfterFailedReads", RestartAfterFailedReads.ToString());
            AppendChild(doc, audio, "restartCooldownMs", RestartCooldownMs.ToString());

            Monitor.Save(doc, audio);
        }

        private static string ReadString(XmlNode root, string name, string fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            return node?.InnerText ?? fallback;
        }

        private static int ReadInt(XmlNode root, string name, int fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !int.TryParse(node.InnerText, out int value))
            {
                return fallback;
            }

            return value;
        }

        private static float ReadFloat(XmlNode root, string name, float fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !float.TryParse(node.InnerText, System.Globalization.NumberStyles.Float, System.Globalization.CultureInfo.InvariantCulture, out float value))
            {
                return fallback;
            }

            return value;
        }

        private static void AppendChild(XmlDocument doc, XmlElement root, string name, string value)
        {
            XmlElement child = doc.CreateElement(name);
            child.InnerText = value;
            root.AppendChild(child);
        }
    }

    internal sealed class MonitorSettings
    {
        public bool Enabled { get; set; } = true;
        public string Device { get; set; } = "default";
        public float Gain { get; set; } = 1.0f;

        public static MonitorSettings Load(XmlNode? root)
        {
            MonitorSettings settings = new();
            if (root == null)
            {
                return settings;
            }

            settings.Enabled = ReadBool(root, "enabled", settings.Enabled);
            settings.Device = ReadString(root, "device", settings.Device);
            settings.Gain = ReadFloat(root, "gain", settings.Gain);
            return settings;
        }

        public void Save(XmlDocument doc, XmlElement root)
        {
            XmlElement monitor = doc.CreateElement("monitor");
            root.AppendChild(monitor);

            AppendChild(doc, monitor, "enabled", Enabled ? "true" : "false");
            AppendChild(doc, monitor, "device", Device);
            AppendChild(doc, monitor, "gain", Gain.ToString("0.###", System.Globalization.CultureInfo.InvariantCulture));
        }

        private static bool ReadBool(XmlNode root, string name, bool fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !bool.TryParse(node.InnerText, out bool value))
            {
                return fallback;
            }

            return value;
        }

        private static string ReadString(XmlNode root, string name, string fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            return node?.InnerText ?? fallback;
        }

        private static float ReadFloat(XmlNode root, string name, float fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !float.TryParse(node.InnerText, System.Globalization.NumberStyles.Float, System.Globalization.CultureInfo.InvariantCulture, out float value))
            {
                return fallback;
            }

            return value;
        }

        private static void AppendChild(XmlDocument doc, XmlElement root, string name, string value)
        {
            XmlElement child = doc.CreateElement(name);
            child.InnerText = value;
            root.AppendChild(child);
        }
    }

    internal sealed class PreviewSettings
    {
        public bool Enabled { get; set; } = false;
        public string OutputDevice { get; set; } = "/dev/fb0";
        public List<string> OutputDevices { get; set; } = new();
        public int Width { get; set; }
        public int Height { get; set; }
        public int Fps { get; set; } = 30;
        public string PixelFormat { get; set; } = "rgb565le";

        public static PreviewSettings Load(XmlNode? root, VideoSettings video)
        {
            PreviewSettings settings = new();
            settings.Width = video.Width;
            settings.Height = video.Height;

            if (root == null)
            {
                return settings;
            }

            settings.Enabled = ReadBool(root, "enabled", settings.Enabled);
            XmlNodeList? outputs = root.SelectNodes("output");
            if (outputs != null && outputs.Count > 0)
            {
                foreach (XmlNode node in outputs)
                {
                    if (!string.IsNullOrWhiteSpace(node.InnerText))
                    {
                        settings.OutputDevices.Add(node.InnerText.Trim());
                    }
                }
            }

            settings.OutputDevice = ReadString(root, "output", settings.OutputDevice);
            if (settings.OutputDevices.Count == 0 && !string.IsNullOrWhiteSpace(settings.OutputDevice))
            {
                settings.OutputDevices.Add(settings.OutputDevice);
            }

            settings.Width = ReadInt(root, "width", settings.Width);
            settings.Height = ReadInt(root, "height", settings.Height);
            settings.Fps = ReadInt(root, "fps", settings.Fps);
            settings.PixelFormat = ReadString(root, "pixelFormat", settings.PixelFormat);

            return settings;
        }

        public void Save(XmlDocument doc, XmlElement root)
        {
            XmlElement preview = doc.CreateElement("preview");
            root.AppendChild(preview);

            AppendChild(doc, preview, "enabled", Enabled ? "true" : "false");
            if (OutputDevices.Count > 0)
            {
                foreach (string output in OutputDevices)
                {
                    AppendChild(doc, preview, "output", output);
                }
            }
            else
            {
                AppendChild(doc, preview, "output", OutputDevice);
            }
            AppendChild(doc, preview, "width", Width.ToString());
            AppendChild(doc, preview, "height", Height.ToString());
            AppendChild(doc, preview, "fps", Fps.ToString());
            AppendChild(doc, preview, "pixelFormat", PixelFormat);
        }

        private static bool ReadBool(XmlNode root, string name, bool fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !bool.TryParse(node.InnerText, out bool value))
            {
                return fallback;
            }

            return value;
        }

        private static string ReadString(XmlNode root, string name, string fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            return node?.InnerText ?? fallback;
        }

        private static int ReadInt(XmlNode root, string name, int fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !int.TryParse(node.InnerText, out int value))
            {
                return fallback;
            }

            return value;
        }

        private static void AppendChild(XmlDocument doc, XmlElement root, string name, string value)
        {
            XmlElement child = doc.CreateElement(name);
            child.InnerText = value;
            root.AppendChild(child);
        }
    }

    internal sealed class WebSettings
    {
        public bool Enabled { get; set; } = true;
        public int Port { get; set; } = 8080;

        public static WebSettings Load(XmlNode? root)
        {
            WebSettings settings = new();
            if (root == null)
            {
                return settings;
            }

            settings.Enabled = ReadBool(root, "enabled", settings.Enabled);
            settings.Port = ReadInt(root, "port", settings.Port);
            return settings;
        }

        public void Save(XmlDocument doc, XmlElement root)
        {
            XmlElement web = doc.CreateElement("web");
            root.AppendChild(web);

            AppendChild(doc, web, "enabled", Enabled ? "true" : "false");
            AppendChild(doc, web, "port", Port.ToString());
        }

        private static bool ReadBool(XmlNode root, string name, bool fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !bool.TryParse(node.InnerText, out bool value))
            {
                return fallback;
            }

            return value;
        }

        private static int ReadInt(XmlNode root, string name, int fallback)
        {
            XmlNode? node = root.SelectSingleNode(name);
            if (node == null || !int.TryParse(node.InnerText, out int value))
            {
                return fallback;
            }

            return value;
        }

        private static void AppendChild(XmlDocument doc, XmlElement root, string name, string value)
        {
            XmlElement child = doc.CreateElement(name);
            child.InnerText = value;
            root.AppendChild(child);
        }
    }
}
