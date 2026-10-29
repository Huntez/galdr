package io.github.huntez.galdr

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Surface
import androidx.compose.material3.Slider
import androidx.compose.material3.SliderDefaults
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlin.math.roundToInt

/** Everything the screen shows; no Android state behind it, so it renders in previews. */
data class ScreenState(
    val phase: Status.Phase,
    /** This phone's name, as PCs list it. */
    val name: String = "Pixel 7",
    val peer: String? = null,
    val error: String? = null,
    val address: String? = null,
    /** Read only while drawing the meter, so its 20 updates/s don't recompose the screen. */
    val level: () -> Float = { 0f },
    val processed: Boolean = false,
    val bitrate: Int = 128_000,
    val gain: Float = 1f,
    val port: Int = Protocol.CONTROL_PORT,
    val mics: List<MicChoice> = listOf(MicChoice("", "Auto")),
    val mic: String = "",
    val autoStart: Boolean = false,
    val permissionDenied: Boolean = false,
)

/** What the screen can ask for. Defaults do nothing, for previews. */
interface ScreenActions {
    fun toggle() {}
    fun processed(on: Boolean) {}
    fun bitrate(rate: Int) {}
    fun gain(value: Float) {}
    fun port(port: Int) {}
    fun mic(address: String) {}
    fun autoStart(on: Boolean) {}
}

// Same palette as the PC app (egui dark + galdr accents).
private val Accent = Color(0xFFE6A028)
private val Green = Color(0xFF5ABE6E)
private val Yellow = Color(0xFFDCB43C)
private val Red = Color(0xFFDC5050)
private val Grey = Color(0xFF8C8C8C)
private val Outline = Color(0xFF3C3C3C)

private val colors = darkColorScheme(
    primary = Accent,
    onPrimary = Color.Black,
    background = Color(0xFF1B1B1B),
    surface = Color(0xFF1B1B1B),
    onSurface = Color(0xFFD2D2D2),
    onBackground = Color(0xFFD2D2D2),
    surfaceContainerHighest = Color(0xFF3C3C3C),
    outline = Outline,
)

// Same fonts as the PC app (egui defaults).
private val Ubuntu = FontFamily(Font(R.font.ubuntu_light))
private val Hack = FontFamily(Font(R.font.hack_regular))

private val typography = Typography().run {
    // Ubuntu Light looks small next to Roboto, so every style is a bit larger.
    fun TextStyle.ubuntu() = copy(fontFamily = Ubuntu, fontSize = fontSize * 1.1f, lineHeight = lineHeight * 1.1f)
    copy(
        displayLarge = displayLarge.ubuntu(), displayMedium = displayMedium.ubuntu(),
        displaySmall = displaySmall.ubuntu(), headlineLarge = headlineLarge.ubuntu(),
        headlineMedium = headlineMedium.ubuntu(), headlineSmall = headlineSmall.ubuntu(),
        titleLarge = titleLarge.ubuntu(), titleMedium = titleMedium.ubuntu(),
        titleSmall = titleSmall.ubuntu(), bodyLarge = bodyLarge.ubuntu(),
        bodyMedium = bodyMedium.ubuntu(), bodySmall = bodySmall.ubuntu(),
        labelLarge = labelLarge.ubuntu(), labelMedium = labelMedium.ubuntu(),
        labelSmall = labelSmall.ubuntu(),
    )
}

@Composable
fun GaldrTheme(content: @Composable () -> Unit) =
    MaterialTheme(colorScheme = colors, typography = typography, content = content)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun Screen(state: ScreenState, actions: ScreenActions, advancedOpen: Boolean = false) {
    val running = state.phase != Status.Phase.STOPPED
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        Column(
            Modifier
                .windowInsetsPadding(WindowInsets.safeDrawing)
                .verticalScroll(rememberScrollState())
                .padding(start = 20.dp, end = 20.dp, top = 20.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Column {
                Text("galdr", fontSize = 34.sp, color = Accent)
                Text("Use this phone as a PC microphone", color = Grey)
            }

            // Like the phone's card on the PC: its name first, the address only as a footnote
            // for typing it in by hand (the PC finds the phone by itself).
            Section(null, highlighted = state.phase == Status.Phase.STREAMING) {
                Text(state.name, fontSize = 26.sp)
                Spacer(Modifier.height(12.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    val (dot, text) = when {
                        state.permissionDenied -> Red to "Microphone permission is needed"
                        state.error != null -> Red to "Error: ${state.error}"
                        state.phase == Status.Phase.STOPPED -> Grey to "Stopped"
                        state.phase == Status.Phase.WAITING -> Yellow to "Waiting for the PC…"
                        else -> Green to "Streaming to ${state.peer}"
                    }
                    Box(Modifier.size(10.dp).clip(CircleShape).background(dot))
                    Spacer(Modifier.width(10.dp))
                    Text(text)
                }
                if (state.phase == Status.Phase.STREAMING) {
                    Spacer(Modifier.height(12.dp))
                    LinearProgressIndicator(
                        progress = { state.level().coerceIn(0f, 1f) },
                        modifier = Modifier.fillMaxWidth().height(6.dp),
                        color = Green,
                        trackColor = Outline,
                        drawStopIndicator = {},
                        gapSize = 0.dp,
                    )
                }
                Spacer(Modifier.height(14.dp))
                val address = state.address ?: "no Wi-Fi"
                val port = if (state.port != Protocol.CONTROL_PORT) ":${state.port}" else ""
                Hint("IP $address$port · for Enter IP on the PC")
            }

            Button(
                onClick = actions::toggle,
                modifier = Modifier.fillMaxWidth().height(52.dp),
                shape = RoundedCornerShape(6.dp),
                contentPadding = PaddingValues(horizontal = 16.dp),
                colors = if (running) {
                    ButtonDefaults.buttonColors(
                        containerColor = MaterialTheme.colorScheme.surfaceContainerHighest,
                        contentColor = MaterialTheme.colorScheme.onSurface,
                    )
                } else {
                    ButtonDefaults.buttonColors()
                },
            ) {
                Text(
                    if (running) "Stop" else "Start",
                    fontSize = 18.sp,
                    modifier = Modifier.fillMaxWidth(),
                )
            }

            Section("Settings") {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text("Microphone volume", Modifier.weight(1f))
                    Text("${(state.gain * 100).roundToInt()}%", fontFamily = Hack)
                }
                Slider(
                    value = state.gain,
                    onValueChange = { actions.gain((it * 20).roundToInt() / 20f) }, // 5% steps
                    valueRange = 0f..Settings.MAX_GAIN,
                    colors = SliderDefaults.colors(
                        thumbColor = Accent,
                        activeTrackColor = Accent,
                        inactiveTrackColor = Outline,
                    ),
                )
                Hint("Applies immediately. Watch the level bar for clipping.")
                Spacer(Modifier.height(16.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        Text("Noise suppression")
                        Hint(
                            "Android's noise suppression and gain control. Off gives the raw microphone.",
                        )
                    }
                    Spacer(Modifier.width(12.dp))
                    Switch(
                        checked = state.processed,
                        onCheckedChange = actions::processed,
                        colors = switchColors(),
                    )
                }
            }

            Advanced(state, actions, advancedOpen)
        }
    }
}

/** Collapsible like the PC app's "Advanced". */
@Composable
private fun Advanced(state: ScreenState, actions: ScreenActions, initiallyOpen: Boolean) {
    var open by rememberSaveable { mutableStateOf(initiallyOpen) }
    Column {
        Row(
            Modifier.clickable { open = !open }.padding(vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(if (open) "\u25BE" else "\u25B8", color = Grey)
            Spacer(Modifier.width(8.dp))
            Text("Advanced")
        }
        if (!open) return@Column
        Spacer(Modifier.height(12.dp))

        Text("Microphone")
        Spacer(Modifier.height(8.dp))
        Choice(state.mics, state.mics.first { it.address == state.mic }, { it.label }) { actions.mic(it.address) }
        Spacer(Modifier.height(8.dp))
        Hint("Which built-in microphone to use. Applies immediately.")
        Spacer(Modifier.height(16.dp))

        Text("Quality, kbit/s")
        Spacer(Modifier.height(8.dp))
        Choice(Settings.BITRATES, state.bitrate, { "${it / 1000}" }, actions::bitrate)
        Spacer(Modifier.height(8.dp))
        Hint("128 is plenty for voice. Lower it if the sound stutters on weak Wi-Fi. Applies on the next connection.")
        Spacer(Modifier.height(16.dp))

        var portText by remember(state.port) { mutableStateOf(state.port.toString()) }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("Port", Modifier.weight(1f))
            OutlinedTextField(
                value = portText,
                onValueChange = { text ->
                    portText = text.filter(Char::isDigit).take(5)
                    portText.toIntOrNull()?.takeIf { it in 1..65535 }?.let(actions::port)
                },
                modifier = Modifier.width(120.dp),
                singleLine = true,
                textStyle = TextStyle(fontFamily = Hack, fontSize = 18.sp),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                colors = OutlinedTextFieldDefaults.colors(
                    focusedBorderColor = Accent,
                    unfocusedBorderColor = Outline,
                    cursorColor = Accent,
                ),
            )
        }
        Spacer(Modifier.height(8.dp))
        Hint(
            "Default ${Protocol.CONTROL_PORT}. Galdr on the PC learns it when it finds the phone. " +
                "Applies on the next Start.",
        )
        Spacer(Modifier.height(16.dp))

        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("Start when the app opens", Modifier.weight(1f))
            Switch(checked = state.autoStart, onCheckedChange = actions::autoStart, colors = switchColors())
        }
        Spacer(Modifier.height(8.dp))
        Text(
            "Reset to defaults",
            color = Accent,
            modifier = Modifier
                .clickable {
                    actions.port(Protocol.CONTROL_PORT)
                    actions.mic("")
                    actions.bitrate(128_000)
                    actions.autoStart(false)
                }
                .padding(vertical = 8.dp),
        )
    }
}

/** Segmented single choice in the accent colour. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun <T> Choice(options: List<T>, selected: T, label: (T) -> String, onSelect: (T) -> Unit) {
    SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
        options.forEachIndexed { i, option ->
            SegmentedButton(
                selected = option == selected,
                onClick = { onSelect(option) },
                shape = SegmentedButtonDefaults.itemShape(i, options.size),
                icon = {},
                colors = SegmentedButtonDefaults.colors(
                    activeContainerColor = Accent,
                    activeContentColor = Color.Black,
                    activeBorderColor = Outline,
                    inactiveContainerColor = Color.Transparent,
                    inactiveBorderColor = Outline,
                ),
            ) {
                Text(label(option), fontSize = 15.sp, maxLines = 1)
            }
        }
    }
}

@Composable
private fun switchColors() = SwitchDefaults.colors(
    checkedTrackColor = Accent,
    checkedThumbColor = Color.Black,
    uncheckedTrackColor = Outline,
    uncheckedThumbColor = Grey,
    uncheckedBorderColor = Outline,
)

/** Small grey explanation under a setting. Own line height: the body style's is too tall for 14 sp. */
@Composable
private fun Hint(text: String) = Text(text, color = Grey, fontSize = 14.sp, lineHeight = 19.sp)

/** Outlined group with a grey title, like the PC app's sections. */
@Composable
private fun Section(title: String?, highlighted: Boolean = false, content: @Composable () -> Unit) {
    Column(
        Modifier
            .fillMaxWidth()
            .border(if (highlighted) 1.5.dp else 1.dp, if (highlighted) Accent else Outline, RoundedCornerShape(8.dp))
            .padding(16.dp),
    ) {
        if (title != null) {
            Text(title, color = Grey, fontSize = 15.sp)
            Spacer(Modifier.height(10.dp))
        }
        content()
    }
}

@Composable
private fun PreviewOf(state: ScreenState, advancedOpen: Boolean = false) =
    GaldrTheme { Screen(state, object : ScreenActions {}, advancedOpen) }

@Preview(name = "Stopped", device = "id:pixel_7", showSystemUi = true)
@Composable
private fun StoppedPreview() = PreviewOf(ScreenState(Status.Phase.STOPPED, address = "192.168.178.20"))

@Preview(name = "Waiting", device = "id:pixel_7", showSystemUi = true)
@Composable
private fun WaitingPreview() = PreviewOf(ScreenState(Status.Phase.WAITING, address = "192.168.178.20"))

@Preview(name = "Streaming", device = "id:pixel_7", showSystemUi = true)
@Composable
private fun StreamingPreview() = PreviewOf(
    ScreenState(
        Status.Phase.STREAMING,
        peer = "DESKTOP (192.168.178.25)",
        address = "192.168.178.20",
        level = { 0.45f },
        bitrate = 192_000,
        gain = 1.5f,
        mics = listOf(MicChoice("", "Auto"), MicChoice("bottom", "Bottom"), MicChoice("back", "Back")),
        mic = "back",
    ),
)

@Preview(name = "Advanced", device = "id:pixel_7", heightDp = 1400)
@Composable
private fun AdvancedPreview() = PreviewOf(
    ScreenState(
        Status.Phase.STOPPED,
        address = "192.168.178.20",
        port = 9000,
        mics = listOf(MicChoice("", "Auto"), MicChoice("bottom", "Bottom"), MicChoice("back", "Back")),
        autoStart = true,
    ),
    advancedOpen = true,
)
