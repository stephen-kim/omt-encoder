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
    [JsonSerializable(typeof(FramebufferInfoResponse))]
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

    internal sealed class FramebufferInfoResponse
    {
        public string Name { get; set; } = string.Empty;
        public int Width { get; set; }
        public int Height { get; set; }
    }

}
