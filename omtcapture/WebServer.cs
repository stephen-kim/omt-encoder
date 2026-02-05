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
:root {
  --bg-0: #0b0f14;
  --bg-1: #121923;
  --bg-2: #0f1319;
  --card: #131a24;
  --card-2: #0f141c;
  --border: #273142;
  --muted: #9aa4b2;
  --text: #e5e9f0;
  --accent: #4cc9f0;
  --accent-2: #7cf3c9;
  --danger: #ff6b6b;
  --shadow: 0 20px 40px rgba(0, 0, 0, 0.35);
}

* { box-sizing: border-box; }

body {
  font-family: ""Space Grotesk"", ""IBM Plex Sans"", ""Manrope"", sans-serif;
  margin: 0;
  padding: 28px 16px 48px;
  color: var(--text);
  background:
    radial-gradient(700px 400px at 10% -10%, rgba(76, 201, 240, 0.18), transparent),
    radial-gradient(700px 400px at 90% -10%, rgba(124, 243, 201, 0.14), transparent),
    linear-gradient(180deg, var(--bg-1), var(--bg-0));
  min-height: 100vh;
}

.page {
  max-width: 600px;
  margin: 0 auto;
  display: grid;
  gap: 16px;
}

h1 {
  font-size: 28px;
  margin: 8px 0 6px;
  letter-spacing: 0.2px;
}

h2 {
  font-size: 16px;
  text-transform: uppercase;
  letter-spacing: 1.6px;
  color: var(--muted);
  margin: 0 0 12px;
}

h3 {
  font-size: 16px;
  margin: 14px 0 6px;
  color: var(--text);
}
.section-title {
  display: flex;
  align-items: center;
  gap: 10px;
  margin: 0 0 12px;
}
.section-title .dot {
  width: 8px;
  height: 8px;
  border-radius: 999px;
  background: #4cc9f0;
  box-shadow: 0 0 10px rgba(76, 201, 240, 0.4);
}
.section-title .pill {
  font-size: 11px;
  letter-spacing: 0.2px;
  text-transform: uppercase;
  padding: 4px 8px;
  border-radius: 999px;
  background: rgba(76, 201, 240, 0.16);
  color: #8fe3ff;
  border: 1px solid rgba(76, 201, 240, 0.3);
}
.section-title.outputs .dot {
  background: #ffd666;
  box-shadow: 0 0 10px rgba(255, 214, 102, 0.45);
}
.section-title.outputs .pill {
  background: rgba(255, 214, 102, 0.18);
  color: #ffe08a;
  border-color: rgba(255, 214, 102, 0.35);
}

section {
  background: linear-gradient(180deg, var(--card), var(--card-2));
  border: 1px solid var(--border);
  border-radius: 16px;
  padding: 16px 18px;
  box-shadow: var(--shadow);
}

label {
  display: block;
  font-weight: 600;
  margin-top: 10px;
  color: var(--muted);
  font-size: 13px;
  letter-spacing: 0.2px;
}

input, select, textarea {
  width: 100%;
  padding: 10px 12px;
  margin-top: 6px;
  border-radius: 10px;
  border: 1px solid var(--border);
  background: #0d1219;
  color: var(--text);
  outline: none;
  transition: border-color 140ms ease, box-shadow 140ms ease;
}

input:focus, select:focus, textarea:focus {
  border-color: rgba(76, 201, 240, 0.6);
  box-shadow: 0 0 0 3px rgba(76, 201, 240, 0.14);
}

input[type=checkbox] { width: 18px; height: 18px; margin: 0; accent-color: var(--accent); }

button {
  margin-top: 14px;
  padding: 10px 16px;
  background: linear-gradient(135deg, var(--accent), var(--accent-2));
  color: #071218;
  border: none;
  border-radius: 10px;
  font-weight: 700;
  cursor: pointer;
  box-shadow: 0 10px 24px rgba(76, 201, 240, 0.25);
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

details { margin-top: 10px; }

.check-row { display: flex; align-items: center; gap: 10px; margin-top: 8px; }
.check-row label { margin: 0; font-weight: 600; color: var(--text); }
.check-grid { display: grid; gap: 8px; margin-top: 8px; }
.check-item { display: flex; align-items: center; gap: 10px; font-weight: 600; color: var(--text); }
.two-col { display: grid; grid-template-columns: 1fr 1fr; gap: 16px; margin-top: 6px; }
.col { padding: 12px; border: 1px solid var(--border); border-radius: 12px; background: #0d1219; }
@media (max-width: 720px) {
  .two-col { grid-template-columns: 1fr; }
}

.fade-in { animation: fadeIn 300ms ease-out; }

@keyframes fadeIn {
  from { opacity: 0; transform: translateY(6px); }
  to { opacity: 1; transform: translateY(0); }
}
</style>
</head>
<body>
<div class=""page fade-in"">
  <h1>OMT Capture Control</h1>
  <section>
    <h2>Config</h2>
    <div id=""status""></div>
    <small id=""displayMode""></small>

    <div class=""two-col"">
      <div class=""col"">
        <div class=""section-title"">
          <span class=""dot""></span>
          <span class=""pill"">Inputs</span>
        </div>
        <label>Video device</label>
        <select id=""videoDevicePath""></select>
        <h4>Audio inputs</h4>
        <div id=""audioInputDevices"" class=""check-grid""></div>
        <small>Select one or two inputs to mix. Leave unchecked for no audio.</small>
      </div>
      <div class=""col"">
        <div class=""section-title outputs"">
          <span class=""dot""></span>
          <span class=""pill"">Outputs</span>
        </div>
        <h4>Audio output</h4>
        <div class=""check-row"">
          <input id=""monitorEnabled"" type=""checkbox"" />
          <label for=""monitorEnabled"">Monitor output</label>
        </div>
        <label>Monitor device</label>
        <select id=""monitorDevice""></select>
        <h4>Preview output</h4>
        <div class=""check-row"">
          <input id=""previewEnabled"" type=""checkbox"" />
          <label for=""previewEnabled"">Preview enabled</label>
        </div>
        <label>Preview outputs</label>
        <div id=""previewOutputs"" class=""check-grid""></div>
      </div>
    </div>

    <details>
      <summary>Advanced settings</summary>
      <label>Resolution preset</label>
      <select id=""videoPreset"" onchange=""applyVideoPreset()"">
        <option value=""custom"">Custom</option>
        <option value=""hd-native"">HD (1920x1080, YUY2)</option>
        <option value=""hd-nv12"">HD (1920x1080, NV12)</option>
        <option value=""uhd-yuy2"">UHD (3840x2160, YUY2)</option>
        <option value=""uhd-nv12"">UHD (3840x2160, NV12)</option>
      </select>
      <label>Source name</label>
      <input id=""videoName"" />
      <label>Width</label>
      <input id=""videoWidth"" type=""number"" />
      <label>Height</label>
      <input id=""videoHeight"" type=""number"" />
      <label>Frame rate numerator</label>
      <input id=""videoFrameRateN"" type=""number"" />
      <label>Frame rate denominator</label>
      <input id=""videoFrameRateD"" type=""number"" />
      <div class=""check-row"">
        <input id=""videoUseNative"" type=""checkbox"" onchange=""toggleNativeInputs()"" />
        <label for=""videoUseNative"">Use native input format (no transform)</label>
      </div>
      <label>Codec</label>
      <select id=""videoCodec"">
        <option value=""UYVY"">UYVY</option>
        <option value=""YUY2"">YUY2</option>
        <option value=""NV12"">NV12</option>
      </select>
      <label>Sample rate</label>
      <input id=""audioSampleRate"" type=""number"" />
      <label>Channels</label>
      <input id=""audioChannels"" type=""number"" />
      <label>Mix gain: <span id=""gainDisplay""></span></label>
      <input id=""audioMixGain"" type=""range"" min=""0"" max=""10"" step=""0.1"" oninput=""document.getElementById('gainDisplay').innerText = this.value"" onchange=""saveConfig()"" />
      <label>Monitor gain</label>
      <input id=""monitorGain"" type=""number"" step=""0.01"" />
      <label>Preview fps</label>
      <input id=""previewFps"" type=""number"" />
      <label>Preview pixel format</label>
      <input id=""previewPixelFormat"" />
      <label>Web port</label>
      <input id=""webPort"" type=""number"" />
    </details>

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
      // Use stable name if valid (alphanumeric+), else fall back to index
      // ALSA Names from 'arecord -l' are usually safe short IDs.
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
  document.getElementById('audioMixGain').value = data.audio.mixGain;
  document.getElementById('gainDisplay').innerText = data.audio.mixGain;
  document.getElementById('monitorEnabled').checked = data.audio.monitor.enabled;
  document.getElementById('monitorGain').value = data.audio.monitor.gain;

  document.getElementById('previewEnabled').checked = data.preview.enabled;
  setPreviewOutputs(data.preview.outputDevices || []);
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
  payload.audio.mixGain = Number(document.getElementById('audioMixGain').value);
  payload.audio.monitor.enabled = document.getElementById('monitorEnabled').checked;
  payload.audio.monitor.device = document.getElementById('monitorDevice').value;
  payload.audio.monitor.gain = Number(document.getElementById('monitorGain').value);

  payload.preview.enabled = document.getElementById('previewEnabled').checked;
  payload.preview.outputDevices = getPreviewOutputs();
  payload.preview.fps = Number(document.getElementById('previewFps').value);
  payload.preview.pixelFormat = document.getElementById('previewPixelFormat').value;

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
