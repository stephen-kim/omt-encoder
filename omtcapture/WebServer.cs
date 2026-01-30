using System.Net;
using System.Text;
using System.Text.Json;

namespace omtcapture
{
    internal sealed class WebServer : IDisposable
    {
        private readonly HttpListener _listener = new();
        private readonly Func<Settings> _getSettings;
        private readonly Func<DeviceSnapshot> _getDevices;
        private readonly Func<SettingsUpdate, UpdateResult> _applyUpdate;
        private readonly JsonSerializerOptions _jsonOptions = new(JsonSerializerDefaults.Web)
        {
            WriteIndented = true
        };
        private CancellationTokenSource? _cts;
        private Task? _listenTask;

        public WebServer(int port, Func<Settings> getSettings, Func<DeviceSnapshot> getDevices, Func<SettingsUpdate, UpdateResult> applyUpdate)
        {
            _getSettings = getSettings;
            _getDevices = getDevices;
            _applyUpdate = applyUpdate;
            _listener.Prefixes.Add($"http://*:{port}/");
        }

        public void Start()
        {
            _cts = new CancellationTokenSource();
            _listener.Start();
            _listenTask = Task.Run(() => ListenLoop(_cts.Token));
        }

        public void Dispose()
        {
            if (_cts != null)
            {
                _cts.Cancel();
            }

            try
            {
                _listener.Stop();
            }
            catch
            {
                // Ignore shutdown errors.
            }

            _listener.Close();
            _listenTask?.Wait(TimeSpan.FromSeconds(1));
            _cts?.Dispose();
        }

        private async Task ListenLoop(CancellationToken token)
        {
            while (!token.IsCancellationRequested)
            {
                HttpListenerContext? context = null;
                try
                {
                    context = await _listener.GetContextAsync();
                }
                catch (Exception)
                {
                    if (token.IsCancellationRequested)
                    {
                        return;
                    }
                }

                if (context == null)
                {
                    continue;
                }

                _ = Task.Run(() => HandleRequest(context), token);
            }
        }

        private async Task HandleRequest(HttpListenerContext context)
        {
            try
            {
                string path = context.Request.Url?.AbsolutePath ?? "/";
                switch (path)
                {
                    case "/":
                        await WriteHtml(context);
                        break;
                    case "/api/config":
                        await HandleConfig(context);
                        break;
                    case "/api/devices":
                        await WriteJson(context, _getDevices());
                        break;
                    case "/api/status":
                        await WriteJson(context, new { ok = true });
                        break;
                    default:
                        context.Response.StatusCode = 404;
                        context.Response.Close();
                        break;
                }
            }
            catch (Exception ex)
            {
                context.Response.StatusCode = 500;
                byte[] payload = Encoding.UTF8.GetBytes(ex.Message);
                context.Response.OutputStream.Write(payload, 0, payload.Length);
                context.Response.Close();
            }
        }

        private async Task HandleConfig(HttpListenerContext context)
        {
            if (context.Request.HttpMethod == "GET")
            {
                Settings current = _getSettings();
                SettingsUpdate response = SettingsUpdate.FromSettings(current);
                await WriteJson(context, response);
                return;
            }

            if (context.Request.HttpMethod == "POST")
            {
                using StreamReader reader = new(context.Request.InputStream, context.Request.ContentEncoding);
                string body = await reader.ReadToEndAsync();
                SettingsUpdate? update = JsonSerializer.Deserialize<SettingsUpdate>(body, _jsonOptions);
                if (update == null)
                {
                    context.Response.StatusCode = 400;
                    context.Response.Close();
                    return;
                }

                UpdateResult result = _applyUpdate(update);
                await WriteJson(context, result);
                return;
            }

            context.Response.StatusCode = 405;
            context.Response.Close();
        }

        private Task WriteJson(HttpListenerContext context, object payload)
        {
            string json = JsonSerializer.Serialize(payload, _jsonOptions);
            byte[] buffer = Encoding.UTF8.GetBytes(json);
            context.Response.ContentType = "application/json";
            context.Response.ContentEncoding = Encoding.UTF8;
            return context.Response.OutputStream.WriteAsync(buffer, 0, buffer.Length)
                .ContinueWith(_ => context.Response.Close());
        }

        private Task WriteHtml(HttpListenerContext context)
        {
            string html = @"<!doctype html>
<html>
<head>
<meta charset=""utf-8"" />
<meta name=""viewport"" content=""width=device-width, initial-scale=1"" />
<title>OMT Capture Control</title>
<style>
body { font-family: ""Helvetica"", ""Arial"", sans-serif; margin: 24px; background: #f4f5f7; color: #1b1c1f; }
section { background: #fff; border-radius: 12px; padding: 16px 20px; margin-bottom: 16px; box-shadow: 0 6px 16px rgba(16, 24, 40, 0.08); }
label { display: block; font-weight: 600; margin-top: 10px; }
input, select, textarea { width: 100%; padding: 8px; margin-top: 6px; border-radius: 6px; border: 1px solid #c7c9d1; }
button { margin-top: 12px; padding: 10px 16px; background: #2563eb; color: #fff; border: none; border-radius: 8px; cursor: pointer; }
pre { background: #0f172a; color: #e2e8f0; padding: 12px; border-radius: 8px; overflow: auto; }
small { color: #586174; }
</style>
</head>
<body>
<h1>OMT Capture Control</h1>
<section>
  <h2>Config</h2>
  <div id=""status""></div>

  <h3>Video</h3>
  <label>Source name</label>
  <input id=""videoName"" />
  <label>Video device path</label>
  <input id=""videoDevicePath"" />
  <label>Width</label>
  <input id=""videoWidth"" type=""number"" />
  <label>Height</label>
  <input id=""videoHeight"" type=""number"" />
  <label>Frame rate numerator</label>
  <input id=""videoFrameRateN"" type=""number"" />
  <label>Frame rate denominator</label>
  <input id=""videoFrameRateD"" type=""number"" />
  <label>Codec</label>
  <select id=""videoCodec"">
    <option value=""UYVY"">UYVY</option>
    <option value=""YUY2"">YUY2</option>
    <option value=""NV12"">NV12</option>
  </select>

  <h3>Audio</h3>
  <label>Audio mode</label>
  <select id=""audioMode"">
    <option value=""none"">none</option>
    <option value=""hdmi"">hdmi</option>
    <option value=""trs"">trs</option>
    <option value=""both"">both</option>
  </select>
  <label>HDMI input device (arecord -D)</label>
  <input id=""hdmiDevice"" />
  <label>TRS input device (arecord -D)</label>
  <input id=""trsDevice"" />
  <label>Sample rate</label>
  <input id=""audioSampleRate"" type=""number"" />
  <label>Channels</label>
  <input id=""audioChannels"" type=""number"" />
  <label>Mix gain (when both)</label>
  <input id=""audioMixGain"" type=""number"" step=""0.01"" />
  <label>Monitor output enabled</label>
  <select id=""monitorEnabled"">
    <option value=""true"">true</option>
    <option value=""false"">false</option>
  </select>
  <label>Monitor output device (aplay -D)</label>
  <input id=""monitorDevice"" />
  <label>Monitor gain</label>
  <input id=""monitorGain"" type=""number"" step=""0.01"" />

  <h3>Preview</h3>
  <label>Preview enabled</label>
  <select id=""previewEnabled"">
    <option value=""true"">true</option>
    <option value=""false"">false</option>
  </select>
  <label>Preview output framebuffer (ex: /dev/fb0)</label>
  <input id=""previewOutput"" />
  <label>Preview width</label>
  <input id=""previewWidth"" type=""number"" />
  <label>Preview height</label>
  <input id=""previewHeight"" type=""number"" />
  <label>Preview fps</label>
  <input id=""previewFps"" type=""number"" />
  <label>Preview pixel format</label>
  <input id=""previewPixelFormat"" />

  <h3>Web</h3>
  <label>Web port</label>
  <input id=""webPort"" type=""number"" />

  <button onclick=""saveConfig()"">Save</button>
  <small>Video changes apply without reboot. Web port changes require restart.</small>
</section>
<section>
  <h2>Devices</h2>
  <button onclick=""loadDevices()"">Refresh</button>
  <h3>Audio inputs</h3>
  <pre id=""audioInputs""></pre>
  <h3>Audio outputs</h3>
  <pre id=""audioOutputs""></pre>
  <h3>Video devices</h3>
  <pre id=""videoDevices""></pre>
  <h3>Framebuffers</h3>
  <pre id=""framebuffers""></pre>
</section>
<script>
async function loadConfig() {
  const res = await fetch('/api/config');
  const data = await res.json();
  document.getElementById('videoName').value = data.video.name;
  document.getElementById('videoDevicePath').value = data.video.devicePath;
  document.getElementById('videoWidth').value = data.video.width;
  document.getElementById('videoHeight').value = data.video.height;
  document.getElementById('videoFrameRateN').value = data.video.frameRateN;
  document.getElementById('videoFrameRateD').value = data.video.frameRateD;
  document.getElementById('videoCodec').value = data.video.codec;

  document.getElementById('audioMode').value = data.audio.mode;
  document.getElementById('hdmiDevice').value = data.audio.hdmiDevice;
  document.getElementById('trsDevice').value = data.audio.trsDevice;
  document.getElementById('audioSampleRate').value = data.audio.sampleRate;
  document.getElementById('audioChannels').value = data.audio.channels;
  document.getElementById('audioMixGain').value = data.audio.mixGain;
  document.getElementById('monitorEnabled').value = String(data.audio.monitor.enabled);
  document.getElementById('monitorDevice').value = data.audio.monitor.device;
  document.getElementById('monitorGain').value = data.audio.monitor.gain;

  document.getElementById('previewEnabled').value = String(data.preview.enabled);
  document.getElementById('previewOutput').value = data.preview.outputDevice;
  document.getElementById('previewWidth').value = data.preview.width;
  document.getElementById('previewHeight').value = data.preview.height;
  document.getElementById('previewFps').value = data.preview.fps;
  document.getElementById('previewPixelFormat').value = data.preview.pixelFormat;

  document.getElementById('webPort').value = data.web.port;
}

async function saveConfig() {
  const payload = await (await fetch('/api/config')).json();
  payload.video.name = document.getElementById('videoName').value;
  payload.video.devicePath = document.getElementById('videoDevicePath').value;
  payload.video.width = Number(document.getElementById('videoWidth').value);
  payload.video.height = Number(document.getElementById('videoHeight').value);
  payload.video.frameRateN = Number(document.getElementById('videoFrameRateN').value);
  payload.video.frameRateD = Number(document.getElementById('videoFrameRateD').value);
  payload.video.codec = document.getElementById('videoCodec').value;

  payload.audio.mode = document.getElementById('audioMode').value;
  payload.audio.hdmiDevice = document.getElementById('hdmiDevice').value;
  payload.audio.trsDevice = document.getElementById('trsDevice').value;
  payload.audio.sampleRate = Number(document.getElementById('audioSampleRate').value);
  payload.audio.channels = Number(document.getElementById('audioChannels').value);
  payload.audio.mixGain = Number(document.getElementById('audioMixGain').value);
  payload.audio.monitor.enabled = document.getElementById('monitorEnabled').value === 'true';
  payload.audio.monitor.device = document.getElementById('monitorDevice').value;
  payload.audio.monitor.gain = Number(document.getElementById('monitorGain').value);

  payload.preview.enabled = document.getElementById('previewEnabled').value === 'true';
  payload.preview.outputDevice = document.getElementById('previewOutput').value;
  payload.preview.width = Number(document.getElementById('previewWidth').value);
  payload.preview.height = Number(document.getElementById('previewHeight').value);
  payload.preview.fps = Number(document.getElementById('previewFps').value);
  payload.preview.pixelFormat = document.getElementById('previewPixelFormat').value;

  payload.web.port = Number(document.getElementById('webPort').value);

  const res = await fetch('/api/config', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(payload) });
  const result = await res.json();
  document.getElementById('status').innerText = result.message || 'Saved';
}

async function loadDevices() {
  const res = await fetch('/api/devices');
  const data = await res.json();
  document.getElementById('audioInputs').innerText = data.audioInputs;
  document.getElementById('audioOutputs').innerText = data.audioOutputs;
  document.getElementById('videoDevices').innerText = data.videoDevices.join('\n');
  document.getElementById('framebuffers').innerText = data.framebuffers.join('\n');
}

loadConfig();
loadDevices();
</script>
</body>
</html>";

            byte[] buffer = Encoding.UTF8.GetBytes(html);
            context.Response.ContentType = "text/html";
            context.Response.ContentEncoding = Encoding.UTF8;
            return context.Response.OutputStream.WriteAsync(buffer, 0, buffer.Length)
                .ContinueWith(_ => context.Response.Close());
        }
    }

    internal sealed class SettingsUpdate
    {
        public VideoSettings Video { get; set; } = new();
        public AudioSettings Audio { get; set; } = new();
        public PreviewSettings Preview { get; set; } = new();
        public WebSettings Web { get; set; } = new();

        public static SettingsUpdate FromSettings(Settings settings)
        {
            return new SettingsUpdate
            {
                Video = settings.Video,
                Audio = settings.Audio,
                Preview = settings.Preview,
                Web = settings.Web
            };
        }
    }

    internal sealed class UpdateResult
    {
        public bool Ok { get; set; }
        public string Message { get; set; } = string.Empty;
        public bool VideoRestartRequired { get; set; }
    }
}
