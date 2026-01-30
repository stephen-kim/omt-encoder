/*
* MIT License
*
* Copyright (c) 2025 Open Media Transport Contributors
*
* Permission is hereby granted, free of charge, to any person obtaining a copy
* of this software and associated documentation files (the "Software"), to deal
* in the Software without restriction, including without limitation the rights
* to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
* copies of the Software, and to permit persons to whom the Software is
* furnished to do so, subject to the following conditions:
*
* The above copyright notice and this permission notice shall be included in all
* copies or substantial portions of the Software.
*
* THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
* IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
* FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
* AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
* LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
* OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
* SOFTWARE.
*
*/

using libomtnet;

namespace omtcapture
{
    internal class Program
    {

        static bool running = true;
        static readonly object SendLock = new();
        static readonly object SettingsLock = new();
        static void Main(string[] args)
        {
            try
            {
                Console.CancelKeyPress += Console_CancelKeyPress;
                Console.WriteLine("OMT Capture");

                string configFilename = Path.Combine(AppContext.BaseDirectory, "config.xml");
                if (!File.Exists(configFilename))
                {
                    Settings defaults = new Settings();
                    defaults.Save(configFilename);
                    Console.WriteLine("config.xml not found. Created default config.");
                }

                Settings settings = Settings.Load(configFilename);
                AudioPipeline? audioPipeline = null;
                VideoPipeline? videoPipeline = null;
                WebServer? webServer = null;

                using (OMTSend send = new OMTSend(settings.Video.Name, OMTQuality.Default))
                {
                    audioPipeline = new AudioPipeline(send, SendLock, settings.Audio);
                    audioPipeline.Start();

                    videoPipeline = new VideoPipeline(send, SendLock, settings.Video);
                    videoPipeline.UpdatePreview(settings.Preview);
                    videoPipeline.Start();

                    if (settings.Web.Enabled)
                    {
                        webServer = new WebServer(settings.Web.Port,
                            () => settings,
                            DeviceProbe.GetSnapshot,
                            update => ApplyUpdate(update, ref settings, configFilename, send, ref audioPipeline, ref videoPipeline));
                        webServer.Start();
                    }

                    while (running)
                    {
                        Thread.Sleep(200);
                    }
                }

                audioPipeline?.Stop();
                videoPipeline?.Stop();
                webServer?.Dispose();
            }
            catch (Exception ex)
            {
                Console.WriteLine(ex.ToString());
            }            

        }

        private static void Console_CancelKeyPress(object? sender, ConsoleCancelEventArgs e)
        {
            e.Cancel = true;
            running = false;
        }

        private static UpdateResult ApplyUpdate(
            SettingsUpdate update,
            ref Settings settings,
            string configPath,
            OMTSend send,
            ref AudioPipeline? audioPipeline,
            ref VideoPipeline? videoPipeline)
        {
            bool videoChanged;
            bool audioChanged;
            bool previewChanged;
            bool webChanged;
            bool nameChanged;

            lock (SettingsLock)
            {
                videoChanged = !VideoEquals(settings.Video, update.Video);
                audioChanged = !AudioEquals(settings.Audio, update.Audio);
                previewChanged = !PreviewEquals(settings.Preview, update.Preview);
                webChanged = !WebEquals(settings.Web, update.Web);
                nameChanged = settings.Video.Name != update.Video.Name;

                CopyVideo(settings.Video, update.Video);
                CopyAudio(settings.Audio, update.Audio);
                CopyPreview(settings.Preview, update.Preview);
                CopyWeb(settings.Web, update.Web);

                settings.Save(configPath);
            }

            if (audioChanged)
            {
                audioPipeline?.Stop();
                audioPipeline = new AudioPipeline(send, SendLock, settings.Audio);
                audioPipeline.Start();
            }

            if (previewChanged)
            {
                videoPipeline?.UpdatePreview(settings.Preview);
            }

            if (videoChanged)
            {
                videoPipeline?.Update(settings.Video);
            }

            return new UpdateResult
            {
                Ok = true,
                VideoRestartRequired = false,
                Message = BuildUpdateMessage(videoChanged, webChanged, nameChanged)
            };
        }

        private static string BuildUpdateMessage(bool videoChanged, bool webChanged, bool nameChanged)
        {
            if (webChanged)
            {
                return "Saved. Web port changes require restart.";
            }

            if (videoChanged && nameChanged)
            {
                return "Saved. Video updated. Source name change requires restart.";
            }

            return "Saved. Changes applied.";
        }

        private static bool VideoEquals(VideoSettings left, VideoSettings right)
        {
            return left.Name == right.Name &&
                left.DevicePath == right.DevicePath &&
                left.Width == right.Width &&
                left.Height == right.Height &&
                left.FrameRateN == right.FrameRateN &&
                left.FrameRateD == right.FrameRateD &&
                left.Codec == right.Codec;
        }

        private static bool AudioEquals(AudioSettings left, AudioSettings right)
        {
            return left.Mode == right.Mode &&
                left.HdmiDevice == right.HdmiDevice &&
                left.TrsDevice == right.TrsDevice &&
                left.SampleRate == right.SampleRate &&
                left.Channels == right.Channels &&
                left.SamplesPerChannel == right.SamplesPerChannel &&
                Math.Abs(left.MixGain - right.MixGain) < 0.0001f &&
                MonitorEquals(left.Monitor, right.Monitor);
        }

        private static bool MonitorEquals(MonitorSettings left, MonitorSettings right)
        {
            return left.Enabled == right.Enabled &&
                left.Device == right.Device &&
                Math.Abs(left.Gain - right.Gain) < 0.0001f;
        }

        private static bool PreviewEquals(PreviewSettings left, PreviewSettings right)
        {
            return left.Enabled == right.Enabled &&
                left.OutputDevice == right.OutputDevice &&
                left.OutputDevices.SequenceEqual(right.OutputDevices) &&
                left.Width == right.Width &&
                left.Height == right.Height &&
                left.Fps == right.Fps &&
                left.PixelFormat == right.PixelFormat;
        }

        private static bool WebEquals(WebSettings left, WebSettings right)
        {
            return left.Enabled == right.Enabled &&
                left.Port == right.Port;
        }

        private static void CopyVideo(VideoSettings target, VideoSettings source)
        {
            target.Name = source.Name;
            target.DevicePath = source.DevicePath;
            target.Width = source.Width;
            target.Height = source.Height;
            target.FrameRateN = source.FrameRateN;
            target.FrameRateD = source.FrameRateD;
            target.Codec = source.Codec;
        }

        private static void CopyAudio(AudioSettings target, AudioSettings source)
        {
            target.Mode = source.Mode;
            target.HdmiDevice = source.HdmiDevice;
            target.TrsDevice = source.TrsDevice;
            target.SampleRate = source.SampleRate;
            target.Channels = source.Channels;
            target.SamplesPerChannel = source.SamplesPerChannel;
            target.MixGain = source.MixGain;

            target.Monitor.Enabled = source.Monitor.Enabled;
            target.Monitor.Device = source.Monitor.Device;
            target.Monitor.Gain = source.Monitor.Gain;
        }

        private static void CopyPreview(PreviewSettings target, PreviewSettings source)
        {
            target.Enabled = source.Enabled;
            target.OutputDevice = source.OutputDevice;
            target.OutputDevices = new List<string>(source.OutputDevices);
            target.Width = source.Width;
            target.Height = source.Height;
            target.Fps = source.Fps;
            target.PixelFormat = source.PixelFormat;
        }

        private static void CopyWeb(WebSettings target, WebSettings source)
        {
            target.Enabled = source.Enabled;
            target.Port = source.Port;
        }
    }
}
