package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.theme.Radii
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.promtuz.chat.domain.model.MessageContent
import com.promtuz.chat.domain.model.SendStatus
import com.promtuz.chat.domain.model.UiMessage
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.ui.appearance.ChatAppearance
import com.promtuz.chat.ui.appearance.DoubleTapAction
import com.promtuz.chat.ui.appearance.ThemeMode
import com.promtuz.chat.ui.appearance.Wallpaper
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Palette
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.PromtuzColors
import com.promtuz.chat.ui.theme.colors

@Composable
fun ChatAppearanceScreen() {
    val appearance by AppearanceStore.appearance.collectAsState()
    var confirmReset by remember { mutableStateOf(false) }
    val pattern = appearance.wallpaper as? Wallpaper.Pattern ?: Wallpaper.Pattern()

    GroupedScreen({ Text("Appearance") }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            PreviewCard(appearance)
            CardGap()

            SettingsCard(header = "Theme") {
                ThemeTiles(appearance.themeMode, Modifier) { set { copy(themeMode = it) } }
                SettingsChoiceRow(
                    "Palette",
                    choices = Palette.entries.map { Choice(it, it.label, PaletteNotes[it]) },
                    selected = appearance.palette,
                    onSelect = { set { copy(palette = it) } },
                )
            }
            CardGap()

            SettingsCard(header = "Colors") {
                SwatchBlock("Accent", appearance.colors.accent, Promtuz.colors.accentFill, AccentPalette, Modifier) {
                    set { copy(colors = colors.copy(accent = it)) }
                }
                SwatchBlock("Your bubbles", appearance.colors.outgoing, Promtuz.colors.bubbleOut, BubblePalette, Modifier) {
                    set { copy(colors = colors.copy(outgoing = it, outgoingText = null)) }
                }
            }
            CardGap()

            SettingsCard(header = "Messages") {
                SliderBlock("Text size", appearance.type.fontScale, 0.85f..1.3f, ::percent, Modifier) {
                    set { copy(type = type.copy(fontScale = it)) }
                }
                SliderBlock("Corner roundness", appearance.bubble.cornerRadius, 2f..24f, { "${it.toInt()} dp" }, Modifier, steps = 21) {
                    set { copy(bubble = bubble.copy(cornerRadius = it)) }
                }
                SettingsSwitchRow("Bubble tail", appearance.bubble.tail, { set { copy(bubble = bubble.copy(tail = it)) } })
            }
            CardGap()

            SettingsCard(header = "Wallpaper") {
                SliderBlock("Pattern", pattern.alpha, 0f..0.35f, ::percent, Modifier) {
                    set { copy(wallpaper = pattern.copy(alpha = it)) }
                }
                SettingsChoiceRow(
                    "Tint",
                    choices = listOf(Choice<Long?>(null, "Default", "Follows the theme")) +
                        TintPalette.map { (name, argb) -> Choice<Long?>(argb, name) },
                    selected = pattern.tintArgb,
                    onSelect = { set { copy(wallpaper = pattern.copy(tintArgb = it)) } },
                )
            }
            CardGap()

            SettingsCard(header = "Interaction") {
                SettingsChoiceRow(
                    "Double tap a message",
                    choices = listOf(
                        Choice(DoubleTapAction.React, "React", "Adds your quick reaction"),
                        Choice(DoubleTapAction.Reply, "Reply", "Starts a reply to it"),
                        Choice(DoubleTapAction.Edit, "Edit", "Opens your own message for editing"),
                        Choice(DoubleTapAction.None, "Nothing"),
                    ),
                    selected = appearance.interaction.doubleTapAction,
                    onSelect = { set { copy(interaction = interaction.copy(doubleTapAction = it)) } },
                )
                if (appearance.interaction.doubleTapAction == DoubleTapAction.React) SettingsChoiceRow(
                    "Quick reaction",
                    choices = QuickReactions.map { Choice(it, it) },
                    selected = appearance.interaction.doubleTapEmoji,
                    onSelect = { set { copy(interaction = interaction.copy(doubleTapEmoji = it)) } },
                )
            }
            CardGap()

            SettingsCard {
                SettingsRow("Reset to defaults", titleColor = Promtuz.colors.danger, onClick = { confirmReset = true })
            }
        }
    }

    if (confirmReset) AppAlertDialog(
        onDismissRequest = { confirmReset = false },
        title = { Text("Reset appearance?") },
        text = { Text("Theme, palette, colors, bubbles and wallpaper go back to how they started.") },
        confirmButton = {
            TextButton(onClick = { AppearanceStore.update { ChatAppearance.Default }; confirmReset = false }) {
                Text("Reset", color = MaterialTheme.colorScheme.error)
            }
        },
        dismissButton = { TextButton(onClick = { confirmReset = false }) { Text("Cancel") } },
    )
}

private fun set(transform: ChatAppearance.() -> ChatAppearance) = AppearanceStore.update { it.transform() }

private fun percent(v: Float) = "${Math.round(v * 100)}%"

private val PaletteNotes = mapOf(
    Palette.Graphite to "Neutral greys with the logo’s cyan",
    Palette.Classic to "Blue-grey surfaces, softer blue",
    Palette.Ink to "Deep navy with a brighter accent",
)

private val AccentPalette = listOf(0xFF7C6CF0, 0xFFB06CF0, 0xFFF06CB4, 0xFFF0756C, 0xFFF0A93C, 0xFF3CC98A, 0xFF3CB8C9)

private val BubblePalette = listOf(0xFF4F5AA8, 0xFF7A4FA8, 0xFFA84F8C, 0xFFA84F4F, 0xFFA8763A, 0xFF3D8F62, 0xFF3A8FA8)

private val TintPalette = listOf(
    "Blue" to 0xFF3A6EA8, "Violet" to 0xFF7A4FA8, "Rose" to 0xFFA84F8C,
    "Amber" to 0xFFA8763A, "Green" to 0xFF3D8F62, "Teal" to 0xFF3A8FA8,
)

private val QuickReactions = listOf("❤️", "👍", "🔥", "😂", "😮", "🎉")


@Composable
private fun PreviewCard(appearance: ChatAppearance) {
    val now = remember { System.currentTimeMillis() }
    val (incoming, outgoing) = remember(now) {
        fun msg(id: String, text: String, out: Boolean) = UiMessage(
            key = id, localId = id, dispatchIdHex = id,
            content = MessageContent.Text(text), outgoing = out,
            status = SendStatus.Read, edited = false, deleted = false,
            timestampMs = now, reactions = emptyList(),
        )
        msg("p1", "Hey! How’s the new look?", false) to msg("p2", "Fresh. Everything here is tweakable", true)
    }
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radii.card))
            .then(rememberChatWallpaper(appearance.wallpaper))
            .padding(vertical = 12.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        MessageBubble(msg = incoming)
        MessageBubble(msg = outgoing)
    }
}

/** Three small chat previews; System is split corner to corner between light and dark. */
@Composable
private fun ThemeTiles(selected: ThemeMode, modifier: Modifier, onPick: (ThemeMode) -> Unit) {
    val c = Promtuz.colors
    val palette = AppearanceStore.appearance.collectAsState().value.palette
    val light = remember(palette) { palette.colors(false) }
    val dark = remember(palette) { palette.colors(true) }
    Row(
        modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 6.dp, bottom = 14.dp),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        ThemeMode.entries.forEach { mode ->
            val on = mode == selected
            Column(
                Modifier.weight(1f).clip(RoundedCornerShape(Radii.badge))
                    .clickable(role = Role.RadioButton) { onPick(mode) },
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                val shape = RoundedCornerShape(Radii.badge)
                Box(
                    Modifier.fillMaxWidth().height(72.dp).clip(shape)
                        .border(if (on) 2.5.dp else 0.dp, if (on) c.accentFill else Color.Transparent, shape),
                ) {
                    when (mode) {
                        ThemeMode.Light -> MiniChat(light, Modifier.matchParentSize())
                        ThemeMode.Dark -> MiniChat(dark, Modifier.matchParentSize())
                        ThemeMode.System -> {
                            MiniChat(dark, Modifier.matchParentSize())
                            // The light half covers the top-left triangle.
                            Box(Modifier.matchParentSize().clip(DiagonalHalf)) { MiniChat(light, Modifier.matchParentSize()) }
                        }
                    }
                }
                Text(
                    mode.name,
                    Modifier.padding(bottom = 2.dp),
                    style = RowTitleStyle.copy(fontWeight = if (on) FontWeight.Medium else FontWeight.Normal),
                    color = if (on) c.accent else c.text,
                )
            }
        }
    }
}

private val DiagonalHalf = androidx.compose.foundation.shape.GenericShape { size, _ ->
    moveTo(0f, 0f); lineTo(size.width, 0f); lineTo(0f, size.height); close()
}

@Composable
private fun MiniChat(p: PromtuzColors, modifier: Modifier) {
    Box(modifier.background(p.wallpaper).padding(12.dp)) {
        Box(Modifier.align(Alignment.TopStart).width(46.dp).height(14.dp).clip(RoundedCornerShape(7.dp)).background(p.bubbleIn))
        Box(Modifier.align(Alignment.BottomEnd).width(52.dp).height(14.dp).clip(RoundedCornerShape(7.dp)).background(p.bubbleOut))
    }
}

/** A labelled row of colour picks; the first is the theme's own colour and clears the override. */
@Composable
private fun SwatchBlock(
    label: String,
    selected: Long?,
    themeColor: Color,
    palette: List<Long>,
    modifier: Modifier,
    onPick: (Long?) -> Unit,
) {
    Column(modifier.fillMaxWidth().padding(top = 8.dp, bottom = 12.dp)) {
        Text(label, Modifier.padding(horizontal = 16.dp), style = RowTitleStyle, color = Promtuz.colors.text)
        Row(
            Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 8.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Swatch(themeColor, selected == null, "Default") { onPick(null) }
            palette.forEach { argb -> Swatch(Color(argb), selected == argb, null) { onPick(argb) } }
        }
    }
}

@Composable
private fun Swatch(fill: Color, selected: Boolean, name: String?, onClick: () -> Unit) {
    val c = Promtuz.colors
    val outer = 12.dp
    val ring = 4.dp
    Box(
        Modifier.size(38.dp).clip(RoundedCornerShape(outer))
            .border(2.dp, if (selected) c.accentFill else Color.Transparent, RoundedCornerShape(outer))
            .clickable(role = Role.RadioButton, onClick = onClick)
            .semantics { name?.let { contentDescription = it } }
            .padding(ring)
            .clip(RoundedCornerShape(concentric(outer, ring)))
            .background(fill),
    )
}

@Composable
private fun SliderBlock(
    label: String,
    value: Float,
    range: ClosedFloatingPointRange<Float>,
    display: (Float) -> String,
    modifier: Modifier,
    steps: Int = 0,
    onChange: (Float) -> Unit,
) {
    val c = Promtuz.colors
    Column(modifier.fillMaxWidth().padding(start = 16.dp, end = 16.dp, top = 12.dp, bottom = 10.dp)) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Text(label, Modifier.weight(1f), style = RowTitleStyle, color = c.text)
            Text(display(value), style = RowValueStyle, color = c.accent)
        }
        AppSlider(value, onChange, Modifier.padding(top = 6.dp), range = range, steps = steps)
    }
}
