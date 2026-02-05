using System.Net;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization.Metadata;

namespace omtcapture
{
    internal sealed class WebServer : IDisposable
    {
        private readonly HttpListener _listener = new();
        private readonly Func<Settings> _getSettings;
        private readonly Func<DeviceSnapshot> _getDevices;
        private readonly Func<SettingsUpdate, UpdateResult> _applyUpdate;
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
                    case "/api/fbname":
                        await HandleFramebufferName(context);
                        break;
                    case "/api/fbinfo":
                        await HandleFramebufferInfo(context);
                        break;
                    case "/api/status":
                        await WriteJson(context, new StatusResponse { Ok = true });
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
                SettingsUpdate? update = JsonSerializer.Deserialize(body, OmcJsonContext.Default.SettingsUpdate);
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

        private Task HandleFramebufferName(HttpListenerContext context)
        {
            string? path = context.Request.QueryString["path"];
            if (string.IsNullOrWhiteSpace(path))
            {
                context.Response.StatusCode = 400;
                context.Response.Close();
                return Task.CompletedTask;
            }

            string? name = null;
            try
            {
                string fb = Path.GetFileName(path);
                if (fb.StartsWith("fb", StringComparison.OrdinalIgnoreCase))
                {
                    string namePath = Path.Combine("/sys/class/graphics", fb, "name");
                    if (File.Exists(namePath))
                    {
                        name = File.ReadAllText(namePath).Trim();
                    }
                }
            }
            catch
            {
                // ignore
            }

            return WriteJson(context, new FramebufferNameResponse { Name = name ?? string.Empty });
        }

        private Task HandleFramebufferInfo(HttpListenerContext context)
        {
            string? path = context.Request.QueryString["path"];
            if (string.IsNullOrWhiteSpace(path))
            {
                context.Response.StatusCode = 400;
                context.Response.Close();
                return Task.CompletedTask;
            }

            string name = string.Empty;
            int width = 0;
            int height = 0;
            try
            {
                string fb = Path.GetFileName(path);
                if (fb.StartsWith("fb", StringComparison.OrdinalIgnoreCase))
                {
                    string basePath = Path.Combine("/sys/class/graphics", fb);
                    string namePath = Path.Combine(basePath, "name");
                    if (File.Exists(namePath))
                    {
                        name = File.ReadAllText(namePath).Trim();
                    }

                    string sizePath = Path.Combine(basePath, "virtual_size");
                    if (File.Exists(sizePath))
                    {
                        string[] parts = File.ReadAllText(sizePath).Trim().Split(',');
                        if (parts.Length == 2)
                        {
                            int.TryParse(parts[0], out width);
                            int.TryParse(parts[1], out height);
                        }
                    }
                }
            }
            catch
            {
                // ignore
            }

            return WriteJson(context, new FramebufferInfoResponse
            {
                Name = name,
                Width = width,
                Height = height
            });
        }

        private Task WriteJson(HttpListenerContext context, object payload)
        {
            JsonTypeInfo? typeInfo = GetTypeInfo(payload);
            if (typeInfo == null)
            {
                context.Response.StatusCode = 500;
                context.Response.Close();
                return Task.CompletedTask;
            }

            string json = JsonSerializer.Serialize(payload, typeInfo);
            byte[] buffer = Encoding.UTF8.GetBytes(json);
            context.Response.ContentType = "application/json";
            context.Response.ContentEncoding = Encoding.UTF8;
            return context.Response.OutputStream.WriteAsync(buffer, 0, buffer.Length)
                .ContinueWith(_ => context.Response.Close());
        }

        private static JsonTypeInfo? GetTypeInfo(object payload)
        {
            return payload switch
            {
                SettingsUpdate => OmcJsonContext.Default.SettingsUpdate,
                UpdateResult => OmcJsonContext.Default.UpdateResult,
                DeviceSnapshot => OmcJsonContext.Default.DeviceSnapshot,
                StatusResponse => OmcJsonContext.Default.StatusResponse,
                FramebufferNameResponse => OmcJsonContext.Default.FramebufferNameResponse,
                FramebufferInfoResponse => OmcJsonContext.Default.FramebufferInfoResponse,
                _ => null
            };
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
@import url('https://fonts.googleapis.com/css2?family=Sora:wght@300;400;500;600;700&family=Space+Grotesk:wght@400;500;600;700&display=swap');
:root {
  --bg-0: #070a0f;
  --bg-1: #0c121a;
  --bg-2: #101824;
  --card: #0f1722;
  --card-2: #101b28;
  --border: #1f2a3a;
  --muted: #9aa4b2;
  --text: #e7eef8;
  --accent: #ffb347;
  --accent-2: #56d4ff;
  --accent-3: #6ef7c7;
  --danger: #ff6b6b;
  --shadow: 0 22px 50px rgba(4, 8, 16, 0.5);
}

* { box-sizing: border-box; }

body {
  font-family: ""Sora"", ""Space Grotesk"", ""Work Sans"", system-ui, -apple-system, sans-serif;
  margin: 0;
  padding: 32px 16px 56px;
  color: var(--text);
  background:
    radial-gradient(800px 440px at 12% -10%, rgba(86, 212, 255, 0.16), transparent 70%),
    radial-gradient(900px 520px at 92% -20%, rgba(110, 247, 199, 0.14), transparent 70%),
    linear-gradient(180deg, var(--bg-2), var(--bg-0));
  min-height: 100vh;
}

.page {
  max-width: 980px;
  margin: 0 auto;
  display: grid;
  gap: 20px;
}

.hero {
  padding: 18px 20px;
  border-radius: 18px;
  background: linear-gradient(135deg, rgba(255, 179, 71, 0.12), rgba(86, 212, 255, 0.08));
  border: 1px solid rgba(86, 212, 255, 0.18);
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 18px;
  box-shadow: var(--shadow);
}

.kicker {
  text-transform: uppercase;
  letter-spacing: 2px;
  color: var(--muted);
  font-size: 11px;
  margin: 0 0 6px;
}

h1 {
  font-size: 28px;
  margin: 0 0 6px;
}

.sub {
  margin: 0;
  color: var(--muted);
  font-size: 14px;
}

.status-pill {
  padding: 8px 12px;
  border-radius: 999px;
  background: rgba(86, 212, 255, 0.14);
  border: 1px solid rgba(86, 212, 255, 0.35);
  font-weight: 600;
  font-size: 12px;
}

.meta {
  margin-top: 6px;
  font-size: 12px;
  color: var(--muted);
}

.card {
  background: linear-gradient(180deg, var(--card), var(--card-2));
  border: 1px solid var(--border);
  border-radius: 16px;
  padding: 16px 18px;
  box-shadow: var(--shadow);
}

.card-title {
  font-size: 13px;
  text-transform: uppercase;
  letter-spacing: 1.6px;
  color: var(--muted);
  margin-bottom: 10px;
}

.section-label {
  font-size: 12px;
  font-weight: 600;
  color: var(--muted);
  margin-top: 12px;
}

label {
  display: block;
  font-weight: 600;
  margin-top: 10px;
  color: var(--text);
  font-size: 13px;
}

.help {
  color: var(--muted);
  font-size: 12px;
  margin-top: 6px;
  line-height: 1.4;
}

input, select, textarea {
  width: 100%;
  padding: 10px 12px;
  margin-top: 6px;
  border-radius: 10px;
  border: 1px solid var(--border);
  background: #0c1219;
  color: var(--text);
  outline: none;
  transition: border-color 140ms ease, box-shadow 140ms ease;
}

input:focus, select:focus, textarea:focus {
  border-color: rgba(86, 212, 255, 0.6);
  box-shadow: 0 0 0 3px rgba(86, 212, 255, 0.12);
}

input[type=checkbox] { width: 18px; height: 18px; margin: 0; accent-color: var(--accent-2); }

button {
  margin-top: 14px;
  padding: 10px 16px;
  background: linear-gradient(135deg, var(--accent), var(--accent-2));
  color: #0b1119;
  border: none;
  border-radius: 10px;
  font-weight: 700;
  cursor: pointer;
  box-shadow: 0 10px 24px rgba(86, 212, 255, 0.25);
}

button:hover { transform: translateY(-1px); }

pre {
  background: #0b0f14;
  color: #cbd5e1;
  padding: 12px;
  border-radius: 10px;
  border: 1px solid #1f2734;
  overflow: auto;
}

small { color: var(--muted); display: block; margin-top: 8px; }

.check-row { display: flex; align-items: center; gap: 10px; margin-top: 8px; }
.check-row label { margin: 0; font-weight: 600; color: var(--text); }
.check-grid { display: grid; gap: 8px; margin-top: 8px; }
.check-item { display: flex; align-items: center; gap: 10px; font-weight: 600; color: var(--text); }

.grid {
  display: grid;
  gap: 16px;
}

.grid.two {
  grid-template-columns: repeat(2, minmax(0, 1fr));
}

.inline {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 10px;
}

.inline.three {
  grid-template-columns: repeat(3, minmax(0, 1fr));
}

@media (max-width: 900px) {
  .grid.two { grid-template-columns: 1fr; }
  .inline, .inline.three { grid-template-columns: 1fr; }
  .hero { flex-direction: column; align-items: flex-start; }
}

.fade-in { animation: fadeIn 320ms ease-out; }

@keyframes fadeIn {
  from { opacity: 0; transform: translateY(8px); }
  to { opacity: 1; transform: translateY(0); }
}
</style>
</head>
<body>
<div class=""page fade-in"">
  <header class=""hero"">
    <div>
      <div class=""kicker"">OMT Capture</div>
      <h1>Control Deck</h1>
      <p class=""sub"">Low-latency ingest, transport, and monitoring controls.</p>
    </div>
    <div>
      <div id=""status"" class=""status-pill"">Loading…</div>
      <div id=""displayMode"" class=""meta""></div>
    </div>
  </header>

  <section class=""grid two"">
    <div class=""card"">
      <div class=""card-title"">Video Input</div>
      <label>Video device</label>
      <select id=""videoDevicePath""></select>
      <div class=""help"">V4L2 device that provides HDMI capture.</div>

      <label>Source name</label>
      <input id=""videoName"" />
      <div class=""help"">Shown in the receiver list. Keep it short and unique.</div>

      <div class=""check-row"">
        <input id=""videoUseNative"" type=""checkbox"" onchange=""toggleNativeInputs()"" />
        <label for=""videoUseNative"">Use native input format (no transform)</label>
      </div>
      <div class=""help"">Bypass scaling and pixel conversion for lowest latency.</div>

      <label>Resolution preset</label>
      <select id=""videoPreset"" onchange=""applyVideoPreset()"">
        <option value=""custom"">Custom</option>
        <option value=""hd-native"">HD (1920x1080, YUY2)</option>
        <option value=""hd-nv12"">HD (1920x1080, NV12)</option>
        <option value=""uhd-yuy2"">UHD (3840x2160, YUY2)</option>
        <option value=""uhd-nv12"">UHD (3840x2160, NV12)</option>
      </select>
      <div class=""help"">Choose a preset or dial in a custom format.</div>

      <div class=""inline"">
        <div>
          <label>Width</label>
          <input id=""videoWidth"" type=""number"" />
        </div>
        <div>
          <label>Height</label>
          <input id=""videoHeight"" type=""number"" />
        </div>
      </div>
      <div class=""help"">Output resolution when not using native mode.</div>

      <div class=""inline"">
        <div>
          <label>Frame rate numerator</label>
          <input id=""videoFrameRateN"" type=""number"" />
        </div>
        <div>
          <label>Frame rate denominator</label>
          <input id=""videoFrameRateD"" type=""number"" />
        </div>
      </div>
      <div class=""help"">Example: 30000 / 1001 for 29.97 fps.</div>

      <label>Codec</label>
      <select id=""videoCodec"">
        <option value=""UYVY"">UYVY</option>
        <option value=""YUY2"">YUY2</option>
        <option value=""NV12"">NV12</option>
      </select>
      <div class=""help"">Pixel format sent over OMT when not native.</div>
    </div>

    <div class=""card"">
      <div class=""card-title"">Audio Input</div>
      <div class=""section-label"">Capture sources</div>
      <div id=""audioInputDevices"" class=""check-grid""></div>
      <div class=""help"">Select up to two inputs to mix. Leave empty for silent.</div>

      <div class=""inline"">
        <div>
          <label>Sample rate (Hz)</label>
          <input id=""audioSampleRate"" type=""number"" />
        </div>
        <div>
          <label>Channels</label>
          <input id=""audioChannels"" type=""number"" />
        </div>
      </div>
      <div class=""help"">Keep in sync with the HDMI source when possible.</div>

      <label>Samples per channel</label>
      <input id=""audioSamplesPerChannel"" type=""number"" />
      <div class=""help"">Audio packet size. Lower = lower latency, higher = more stability.</div>

      <label>Mix gain: <span id=""gainDisplay""></span></label>
      <input id=""audioMixGain"" type=""range"" min=""0"" max=""10"" step=""0.1"" oninput=""document.getElementById('gainDisplay').innerText = this.value"" onchange=""saveConfig()"" />
      <div class=""help"">Applied after mixing HDMI/TRS inputs.</div>

      <div class=""section-label"">ALSA tuning</div>
      <div class=""inline"">
        <div>
          <label>arecord buffer (usec)</label>
          <input id=""audioArecordBufferUsec"" type=""number"" />
        </div>
        <div>
          <label>arecord period (usec)</label>
          <input id=""audioArecordPeriodUsec"" type=""number"" />
        </div>
      </div>
      <div class=""help"">Larger buffers reduce underruns but add delay.</div>

      <div class=""inline"">
        <div>
          <label>Restart after failed reads</label>
          <input id=""audioRestartAfterFailedReads"" type=""number"" />
        </div>
        <div>
          <label>Restart cooldown (ms)</label>
          <input id=""audioRestartCooldownMs"" type=""number"" />
        </div>
      </div>
      <div class=""help"">Auto-restarts the audio pipeline when the device stalls.</div>

      <div class=""section-label"">Monitor output</div>
      <div class=""check-row"">
        <input id=""monitorEnabled"" type=""checkbox"" />
        <label for=""monitorEnabled"">Monitor enabled</label>
      </div>
      <label>Monitor device</label>
      <select id=""monitorDevice""></select>
      <label>Monitor gain</label>
      <input id=""monitorGain"" type=""number"" step=""0.01"" />
      <div class=""help"">Applies only to the local monitor output.</div>
    </div>
  </section>

  <section class=""grid two"">
    <div class=""card"">
      <div class=""card-title"">Transport</div>
      <label>Audio queue capacity</label>
      <input id=""sendAudioQueueCapacity"" type=""number"" />
      <div class=""help"">Buffered audio packets before dropping oldest. Helps smooth jitter.</div>

      <label>Video queue capacity</label>
      <input id=""sendVideoQueueCapacity"" type=""number"" />
      <div class=""help"">How many frames can wait before dropping. Keep small for low latency.</div>

      <div class=""check-row"">
        <input id=""sendForceZeroTimestamps"" type=""checkbox"" />
        <label for=""sendForceZeroTimestamps"">Force zero timestamps</label>
      </div>
      <div class=""help"">Reduces receiver buffering. Can break strict A/V sync in some players.</div>
    </div>

    <div class=""card"">
      <div class=""card-title"">Preview</div>
      <div class=""check-row"">
        <input id=""previewEnabled"" type=""checkbox"" />
        <label for=""previewEnabled"">Preview enabled</label>
      </div>
      <div class=""help"">Preview is rendered locally to framebuffer devices.</div>

      <label>Preview outputs</label>
      <div id=""previewOutputs"" class=""check-grid""></div>

      <div class=""inline"">
        <div>
          <label>Preview fps</label>
          <input id=""previewFps"" type=""number"" />
        </div>
        <div>
          <label>Preview pixel format</label>
          <input id=""previewPixelFormat"" />
        </div>
      </div>
      <div class=""help"">Lower preview fps to save CPU when the main stream is stable.</div>
    </div>
  </section>

  <section class=""grid two"">
    <div class=""card"">
      <div class=""card-title"">Web Service</div>
      <label>Web port</label>
      <input id=""webPort"" type=""number"" />
      <div class=""help"">Requires service restart to apply.</div>
      <button onclick=""saveConfig()"">Save changes</button>
      <small>Video changes apply without reboot. Web port changes require restart.</small>
    </div>

    <div class=""card"">
      <div class=""card-title"">Devices</div>
      <button onclick=""loadDevices()"">Refresh</button>
      <div class=""section-label"">Audio inputs</div>
      <pre id=""audioInputs""></pre>
      <div class=""section-label"">Audio outputs</div>
      <pre id=""audioOutputs""></pre>
      <div class=""section-label"">Video devices</div>
      <pre id=""videoDevices""></pre>
      <div class=""section-label"">Framebuffers</div>
      <pre id=""framebuffers""></pre>
    </div>
  </section>
</div>
<script>
function setSelectOptions(selectId, options, selectedValue) {
  const select = document.getElementById(selectId);
  select.innerHTML = '';
  options.forEach(opt => {
    const option = document.createElement('option');
    option.value = opt.value;
    option.textContent = opt.label;
    if (opt.value === selectedValue) {
      option.selected = true;
    }
    select.appendChild(option);
  });
}

function setPreviewOutputs(selected) {
  const container = document.getElementById('previewOutputs');
  const selectedSet = new Set(selected);
  container.querySelectorAll('input[type=checkbox]').forEach(input => {
    input.checked = selectedSet.has(input.value);
  });
}

function getPreviewOutputs() {
  const container = document.getElementById('previewOutputs');
  const outputs = [];
  container.querySelectorAll('input[type=checkbox]').forEach(input => {
    if (input.checked) {
      outputs.push(input.value);
    }
  });
  return outputs;
}

function getAudioInputSelection() {
  const container = document.getElementById('audioInputDevices');
  const outputs = [];
  container.querySelectorAll('input[type=checkbox]').forEach(input => {
    if (input.checked) {
      outputs.push(input.value);
    }
  });
  return outputs;
}

function limitAudioInputs() {
  const container = document.getElementById('audioInputDevices');
  const checked = [];
  container.querySelectorAll('input[type=checkbox]').forEach(input => {
    if (input.checked) {
      checked.push(input);
    }
  });
  if (checked.length <= 2) {
    return;
  }
  checked.slice(2).forEach(input => {
    input.checked = false;
  });
}

function parseAlsaDevices(text) {
  const lines = text.split('\n');
  const devices = [];
  const regex = /card\s+(\d+):\s*([^,]+),\s*device\s+(\d+):\s*([^[]+)/i;
  for (const line of lines) {
    const match = line.match(regex);
    if (match) {
      const card = match[1];
      const device = match[3];
      const name = match[2].trim();
      let value = `hw:${card},${device}`;
      if (/^[a-zA-Z0-9_\-]+$/.test(name)) {
          value = `hw:CARD=${name},DEV=${device}`;
      }
      const devLabel = `${name} (Card ${card}, Dev ${device})`;
      devices.push({ value: value, label: devLabel });
    }
  }
  if (devices.length === 0) {
    devices.push({ value: 'default', label: 'default' });
  }
  return devices;
}

async function buildFramebufferOptions(framebuffers) {
  const options = [];
  for (const fb of framebuffers) {
    let label = fb;
    try {
      const nameRes = await fetch(`/api/fbinfo?path=${encodeURIComponent(fb)}`);
      if (nameRes.ok) {
        const data = await nameRes.json();
        if (data && data.name) {
          label = `${fb} (${data.name})`;
        }
        if (data && data.width && data.height) {
          label = `${label} ${data.width}x${data.height}`;
        }
      }
    } catch (e) {
      // ignore
    }
    options.push({ value: fb, label });
  }
  return options;
}

function applyVideoPreset() {
  const preset = document.getElementById('videoPreset').value;
  if (document.getElementById('videoUseNative').checked) {
    return;
  }
  if (preset === 'hd-native') {
    document.getElementById('videoWidth').value = 1920;
    document.getElementById('videoHeight').value = 1080;
    document.getElementById('videoFrameRateN').value = 30;
    document.getElementById('videoFrameRateD').value = 1;
    document.getElementById('videoCodec').value = 'YUY2';
  } else if (preset === 'hd-nv12') {
    document.getElementById('videoWidth').value = 1920;
    document.getElementById('videoHeight').value = 1080;
    document.getElementById('videoFrameRateN').value = 30;
    document.getElementById('videoFrameRateD').value = 1;
    document.getElementById('videoCodec').value = 'NV12';
  } else if (preset === 'uhd-yuy2') {
    document.getElementById('videoWidth').value = 3840;
    document.getElementById('videoHeight').value = 2160;
    document.getElementById('videoFrameRateN').value = 30;
    document.getElementById('videoFrameRateD').value = 1;
    document.getElementById('videoCodec').value = 'YUY2';
  } else if (preset === 'uhd-nv12') {
    document.getElementById('videoWidth').value = 3840;
    document.getElementById('videoHeight').value = 2160;
    document.getElementById('videoFrameRateN').value = 30;
    document.getElementById('videoFrameRateD').value = 1;
    document.getElementById('videoCodec').value = 'NV12';
  }
}

function toggleNativeInputs() {
  const nativeOn = document.getElementById('videoUseNative').checked;
  const ids = [
    'videoPreset',
    'videoWidth',
    'videoHeight',
    'videoFrameRateN',
    'videoFrameRateD',
    'videoCodec'
  ];
  ids.forEach(id => {
    const el = document.getElementById(id);
    if (el) {
      el.disabled = nativeOn;
    }
  });
}

function detectVideoPreset(width, height) {
  const codec = document.getElementById('videoCodec').value;
  if (width === 1920 && height === 1080 && codec === 'YUY2') {
    return 'hd-native';
  }
  if (width === 1920 && height === 1080 && codec === 'NV12') {
    return 'hd-nv12';
  }
  if (width === 3840 && height === 2160 && codec === 'YUY2') {
    return 'uhd-yuy2';
  }
  if (width === 3840 && height === 2160 && codec === 'NV12') {
    return 'uhd-nv12';
  }
  return 'custom';
}

async function loadDevices() {
  const res = await fetch('/api/devices');
  const data = await res.json();
  const mode = data.displayMode === 'desktop' ? 'Desktop UI running' : 'Console mode';
  document.getElementById('displayMode').innerText = `Display mode: ${mode}`;
  document.getElementById('audioInputs').innerText = data.audioInputs;
  document.getElementById('audioOutputs').innerText = data.audioOutputs;
  document.getElementById('videoDevices').innerText = data.videoDevices.join('\n');
  document.getElementById('framebuffers').innerText = data.framebuffers.join('\n');

  const inputs = parseAlsaDevices(data.audioInputs);
  const outputs = parseAlsaDevices(data.audioOutputs);
  const videoOptions = data.videoDevices.map(v => ({ value: v, label: v }));
  const fbOptions = await buildFramebufferOptions(data.framebuffers);

  const config = await (await fetch('/api/config')).json();
  setSelectOptions('monitorDevice', outputs, config.audio.monitor.device);
  setSelectOptions('videoDevicePath', videoOptions, config.video.devicePath);
  const audioContainer = document.getElementById('audioInputDevices');
  audioContainer.innerHTML = '';
  const selectedAudio = [];
  if (config.audio.mode === 'both') {
    if (config.audio.hdmiDevice) {
      selectedAudio.push(config.audio.hdmiDevice);
    }
    if (config.audio.trsDevice) {
      selectedAudio.push(config.audio.trsDevice);
    }
  } else if (config.audio.mode === 'hdmi' && config.audio.hdmiDevice) {
    selectedAudio.push(config.audio.hdmiDevice);
  } else if (config.audio.mode === 'trs' && config.audio.trsDevice) {
    selectedAudio.push(config.audio.trsDevice);
  }
  const selectedSet = new Set(selectedAudio);
  inputs.forEach(opt => {
    const wrapper = document.createElement('label');
    wrapper.className = 'check-item';
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.value = opt.value;
    checkbox.checked = selectedSet.has(opt.value);
    checkbox.addEventListener('change', limitAudioInputs);
    wrapper.appendChild(checkbox);
    wrapper.appendChild(document.createTextNode(opt.label));
    audioContainer.appendChild(wrapper);
  });
  limitAudioInputs();
  const outputsContainer = document.getElementById('previewOutputs');
  outputsContainer.innerHTML = '';
  fbOptions.forEach(opt => {
    const wrapper = document.createElement('label');
    wrapper.className = 'check-item';
    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.value = opt.value;
    checkbox.checked = (config.preview.outputDevices || []).includes(opt.value);
    wrapper.appendChild(checkbox);
    wrapper.appendChild(document.createTextNode(opt.label));
    outputsContainer.appendChild(wrapper);
  });
}

async function loadConfig() {
  const res = await fetch('/api/config');
  const data = await res.json();
  document.getElementById('videoName').value = data.video.name;
  document.getElementById('videoDevicePath').value = data.video.devicePath;
  document.getElementById('videoWidth').value = data.video.width;
  document.getElementById('videoHeight').value = data.video.height;
  document.getElementById('videoFrameRateN').value = data.video.frameRateN;
  document.getElementById('videoFrameRateD').value = data.video.frameRateD;
  document.getElementById('videoUseNative').checked = data.video.useNativeFormat;
  document.getElementById('videoCodec').value = data.video.codec;
  document.getElementById('videoPreset').value = detectVideoPreset(data.video.width, data.video.height);
  toggleNativeInputs();

  document.getElementById('audioSampleRate').value = data.audio.sampleRate;
  document.getElementById('audioChannels').value = data.audio.channels;
  document.getElementById('audioSamplesPerChannel').value = data.audio.samplesPerChannel;
  document.getElementById('audioMixGain').value = data.audio.mixGain;
  document.getElementById('gainDisplay').innerText = data.audio.mixGain;
  document.getElementById('audioArecordBufferUsec').value = data.audio.arecordBufferUsec;
  document.getElementById('audioArecordPeriodUsec').value = data.audio.arecordPeriodUsec;
  document.getElementById('audioRestartAfterFailedReads').value = data.audio.restartAfterFailedReads;
  document.getElementById('audioRestartCooldownMs').value = data.audio.restartCooldownMs;

  document.getElementById('monitorEnabled').checked = data.audio.monitor.enabled;
  document.getElementById('monitorGain').value = data.audio.monitor.gain;

  document.getElementById('previewEnabled').checked = data.preview.enabled;
  setPreviewOutputs(data.preview.outputDevices || []);
  document.getElementById('previewFps').value = data.preview.fps;
  document.getElementById('previewPixelFormat').value = data.preview.pixelFormat;

  document.getElementById('sendAudioQueueCapacity').value = data.send.audioQueueCapacity;
  document.getElementById('sendVideoQueueCapacity').value = data.send.videoQueueCapacity;
  document.getElementById('sendForceZeroTimestamps').checked = data.send.forceZeroTimestamps;

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
  payload.video.useNativeFormat = document.getElementById('videoUseNative').checked;
  payload.video.codec = document.getElementById('videoCodec').value;

  const selectedInputs = getAudioInputSelection().slice(0, 2);
  if (selectedInputs.length === 0) {
    payload.audio.mode = 'none';
    payload.audio.hdmiDevice = '';
    payload.audio.trsDevice = '';
  } else if (selectedInputs.length === 1) {
    payload.audio.mode = 'hdmi';
    payload.audio.hdmiDevice = selectedInputs[0];
    payload.audio.trsDevice = '';
  } else {
    payload.audio.mode = 'both';
    payload.audio.hdmiDevice = selectedInputs[0];
    payload.audio.trsDevice = selectedInputs[1];
  }
  payload.audio.sampleRate = Number(document.getElementById('audioSampleRate').value);
  payload.audio.channels = Number(document.getElementById('audioChannels').value);
  payload.audio.samplesPerChannel = Number(document.getElementById('audioSamplesPerChannel').value);
  payload.audio.mixGain = Number(document.getElementById('audioMixGain').value);
  payload.audio.arecordBufferUsec = Number(document.getElementById('audioArecordBufferUsec').value);
  payload.audio.arecordPeriodUsec = Number(document.getElementById('audioArecordPeriodUsec').value);
  payload.audio.restartAfterFailedReads = Number(document.getElementById('audioRestartAfterFailedReads').value);
  payload.audio.restartCooldownMs = Number(document.getElementById('audioRestartCooldownMs').value);

  payload.audio.monitor.enabled = document.getElementById('monitorEnabled').checked;
  payload.audio.monitor.device = document.getElementById('monitorDevice').value;
  payload.audio.monitor.gain = Number(document.getElementById('monitorGain').value);

  payload.preview.enabled = document.getElementById('previewEnabled').checked;
  payload.preview.outputDevices = getPreviewOutputs();
  payload.preview.fps = Number(document.getElementById('previewFps').value);
  payload.preview.pixelFormat = document.getElementById('previewPixelFormat').value;

  payload.send.audioQueueCapacity = Number(document.getElementById('sendAudioQueueCapacity').value);
  payload.send.videoQueueCapacity = Number(document.getElementById('sendVideoQueueCapacity').value);
  payload.send.forceZeroTimestamps = document.getElementById('sendForceZeroTimestamps').checked;

  payload.web.port = Number(document.getElementById('webPort').value);

  const res = await fetch('/api/config', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(payload) });
  const result = await res.json();
  document.getElementById('status').innerText = result.message || 'Saved';
}

loadConfig().then(loadDevices);
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
        public SendSettings Send { get; set; } = new();
        public PreviewSettings Preview { get; set; } = new();
        public WebSettings Web { get; set; } = new();

        public static SettingsUpdate FromSettings(Settings settings)
        {
            return new SettingsUpdate
            {
                Video = settings.Video,
                Audio = settings.Audio,
                Send = settings.Send,
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
