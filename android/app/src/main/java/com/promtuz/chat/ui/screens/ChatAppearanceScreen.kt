package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.ui.appearance.ChatAppearance
import com.promtuz.chat.ui.appearance.ThemeMode
import com.promtuz.chat.ui.appearance.Wallpaper
import com.promtuz.chat.ui.components.FlexibleScreen
import com.promtuz.chat.ui.components.MessageBubble
import com.promtuz.chat.ui.components.rememberChatWallpaper

/**
 * The appearance editor: every knob writes the [AppearanceStore] preset directly,
 * so the preview (real bubbles over the real wallpaper) and the whole app restyle
 * live. `null` color tokens mean "auto" — follow the theme.
 */
@Composable
fun ChatAppearanceScreen() {
    val appearance by AppearanceStore.appearance.collectAsState()

    FlexibleScreen({ Text("Chat Appearance") }) { padding, scrollBehavior ->
        Column(
            Modifier
                .fillMaxSize()
                .nestedScroll(scrollBehavior.nestedScrollConnection)
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 18.dp)
                .padding(top = padding.calculateTopPadding() + 12.dp, bottom = 48.dp),
        ) {
            PreviewCard(appearance)

            SectionLabel("Theme")
            SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
                ThemeMode.entries.forEachIndexed { i, mode ->
                    SegmentedButton(
                        selected = appearance.themeMode == mode,
                        onClick = { set { copy(themeMode = mode) } },
                        shape = SegmentedButtonDefaults.itemShape(i, ThemeMode.entries.size),
                    ) { Text(mode.name) }
                }
            }

            SectionLabel("Bubbles")
            SliderRow("Roundness", appearance.bubble.cornerRadius, 2f..24f, { "${it.toInt()} dp" }) {
                set { copy(bubble = bubble.copy(cornerRadius = it)) }
            }
            SwitchRow("Tail", appearance.bubble.tail) {
                set { copy(bubble = bubble.copy(tail = it)) }
            }
            SliderRow("Max width", appearance.layout.maxWidthFraction, 0.6f..0.95f, ::percent) {
                set { copy(layout = layout.copy(maxWidthFraction = it)) }
            }
            SliderRow("Text size", appearance.type.fontScale, 0.85f..1.3f, ::percent) {
                set { copy(type = type.copy(fontScale = it)) }
            }

            SectionLabel("Colors")
            SwatchRow("Outgoing", appearance.colors.outgoing, BubblePalette) {
                set { copy(colors = colors.copy(outgoing = it, outgoingText = null)) }
            }
            SwatchRow("Incoming", appearance.colors.incoming, BubblePalette) {
                set { copy(colors = colors.copy(incoming = it, incomingText = null)) }
            }
            SwatchRow("Accent", appearance.colors.accent, AccentPalette) {
                set { copy(colors = colors.copy(accent = it)) }
            }

            SectionLabel("Wallpaper")
            val pattern = appearance.wallpaper as? Wallpaper.Pattern ?: Wallpaper.Pattern()
            SwatchRow("Tint", pattern.tintArgb, BubblePalette) {
                set { copy(wallpaper = pattern.copy(tintArgb = it)) }
            }
            SliderRow("Pattern", pattern.alpha, 0f..0.35f, ::percent) {
                set { copy(wallpaper = pattern.copy(alpha = it)) }
            }

            TextButton(
                { AppearanceStore.update { ChatAppearance.Default } },
                Modifier.align(Alignment.CenterHorizontally).padding(top = 24.dp),
            ) { Text("Reset to defaults") }
        }
    }
}

private fun set(transform: ChatAppearance.() -> ChatAppearance) =
    AppearanceStore.update { it.transform() }

private fun percent(v: Float) = "${(v * 100).toInt()}%"

/** Muted fills that read well as bubbles/tints in both modes (text auto-derives). */
private val BubblePalette = listOf(
    0xFF3A6EA8, 0xFF4F5AA8, 0xFF7A4FA8, 0xFFA84F8C, 0xFFA84F4F,
    0xFFA8763A, 0xFF3D8F62, 0xFF3A8FA8, 0xFF3C4048,
)

/** Vivid actives for send/cursor/typing. */
private val AccentPalette = listOf(
    0xFF5A91D8, 0xFF7C6CF0, 0xFFB06CF0, 0xFFF06CB4, 0xFFF0756C,
    0xFFF0A93C, 0xFF3CC98A, 0xFF3CB8C9,
)

@Composable
private fun PreviewCard(appearance: ChatAppearance) {
    val now = remember { System.currentTimeMillis() }
    val fakes = remember(now) {
        fun msg(id: String, text: String, out: Boolean) = UiMessage(
            key = id, localId = id, dispatchIdHex = null,
            content = MessageContent.Text(text), outgoing = out,
            status = SendStatus.Read, edited = false, deleted = false,
            timestampMs = now, reactions = emptyList(),
        )
        Triple(
            msg("p1", "Hey! How's the new look? 👀", false),
            msg("p2", "Fresh — everything here is tweakable", true),
            msg("p3", "🔥", true),
        )
    }
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(20.dp))
            .then(rememberChatWallpaper(appearance.wallpaper))
            .padding(vertical = 14.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        MessageBubble(msg = fakes.first)
        MessageBubble(Modifier.padding(top = 6.dp), fakes.second, mergedBottom = true)
        MessageBubble(msg = fakes.third, mergedTop = true)
    }
}

@Composable
private fun SectionLabel(text: String) {
    Text(
        text.uppercase(),
        Modifier.padding(top = 22.dp, bottom = 8.dp, start = 2.dp),
        MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.labelMedium,
    )
}

@Composable
private fun SliderRow(
    label: String,
    value: Float,
    range: ClosedFloatingPointRange<Float>,
    display: (Float) -> String,
    onChange: (Float) -> Unit,
) {
    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
        Text(label, style = MaterialTheme.typography.bodyMedium)
        Text(
            display(value),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
    Slider(value, onChange, Modifier.fillMaxWidth(), valueRange = range)
}

@Composable
private fun SwitchRow(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(vertical = 6.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, style = MaterialTheme.typography.bodyMedium)
        Switch(checked, onChange)
    }
}

/** "Auto" (null = follow theme) + fixed swatches; the picked token is ARGB. */
@Composable
private fun SwatchRow(label: String, selected: Long?, palette: List<Long>, onPick: (Long?) -> Unit) {
    val colors = MaterialTheme.colorScheme
    Text(
        label,
        Modifier.padding(top = 8.dp, bottom = 6.dp),
        style = MaterialTheme.typography.bodyMedium,
    )
    Row(
        Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Swatch(colors.surfaceContainerHigh, selected == null, { onPick(null) }) {
            Text("A", style = MaterialTheme.typography.labelMedium, color = colors.onSurfaceVariant)
        }
        palette.forEach { argb ->
            Swatch(Color(argb), selected == argb, { onPick(argb) })
        }
    }
}

@Composable
private fun Swatch(
    fill: Color,
    selected: Boolean,
    onClick: () -> Unit,
    content: @Composable () -> Unit = {},
) {
    val ring = if (selected) MaterialTheme.colorScheme.primary else Color.Transparent
    Box(
        Modifier
            .size(38.dp)
            .clip(CircleShape)
            .border(2.dp, ring, CircleShape)
            .padding(4.dp)
            .clip(CircleShape)
            .background(fill)
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
        content = { content() },
    )
}
