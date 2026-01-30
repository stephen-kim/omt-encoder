using System.Text.Json.Serialization;

namespace omtcapture
{
    [JsonSourceGenerationOptions(
        PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase,
        WriteIndented = true)]
    [JsonSerializable(typeof(SettingsUpdate))]
    [JsonSerializable(typeof(UpdateResult))]
    [JsonSerializable(typeof(DeviceSnapshot))]
    [JsonSerializable(typeof(StatusResponse))]
    [JsonSerializable(typeof(FramebufferNameResponse))]
    internal partial class OmcJsonContext : JsonSerializerContext
    {
    }

    internal sealed class StatusResponse
    {
        public bool Ok { get; set; }
    }

    internal sealed class FramebufferNameResponse
    {
        public string Name { get; set; } = string.Empty;
    }
}
