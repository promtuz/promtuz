package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Radii
import androidx.annotation.DrawableRes
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.composed
import com.promtuz.chat.ui.util.groupedRoundShape
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.Layout
import androidx.compose.foundation.verticalScroll
import androidx.compose.ui.layout.ParentDataModifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.Promtuz

val RowTitleStyle = TextStyle(fontSize = 16.sp, lineHeight = 22.sp)
val RowSubtitleStyle = TextStyle(fontSize = 13.5.sp, lineHeight = 18.sp)
val RowValueStyle = TextStyle(fontSize = 15.sp, lineHeight = 20.sp)
val SectionHeaderStyle = TextStyle(fontSize = 14.sp, lineHeight = 20.sp, fontWeight = FontWeight.Medium)
val FootnoteStyle = TextStyle(fontSize = 13.sp, lineHeight = 18.sp)

private val Inset = 16.dp
private val Glyph = 24.dp
private val GlyphGap = 18.dp

/** Where the divider above a child starts; null draws none (the card's header, a footnote). */
private class DividerInset(val start: Dp?) : ParentDataModifier {
    override fun Density.modifyParentData(parentData: Any?) = this@DividerInset
}

private fun Modifier.dividerInset(start: Dp?) = this.then(DividerInset(start))

/**
 * One card of settings. Children stack flush and the card draws a divider between neighbours,
 * starting where the lower row's text starts, so rows never track their own position.
 */
@Composable
fun SettingsCard(
    modifier: Modifier = Modifier,
    header: String? = null,
    footer: String? = null,
    content: @Composable () -> Unit,
) {
    val c = Promtuz.colors
    Column(modifier.fillMaxWidth()) {
        val tops = remember { FloatArrayHolder() }
        Layout(
            content = {
                if (header != null) Text(
                    header,
                    Modifier.dividerInset(null).fillMaxWidth()
                        .padding(start = Inset, end = Inset, top = 14.dp, bottom = 2.dp)
                        .semantics { heading() },
                    style = SectionHeaderStyle,
                    color = c.accent,
                )
                content()
            },
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(Radii.card))
                .background(cardFill())
                .drawWithContent {
                    drawContent()
                    val rtl = layoutDirection == LayoutDirection.Rtl
                    for (i in 1 until tops.count) {
                        // Nothing divides a header or a free block from what follows it.
                        val start = tops.insets[i] ?: continue
                        if (tops.insets[i - 1] == null) continue
                        val y = tops.ys[i] - 0.5.dp.toPx()
                        val a = start.toPx(); val b = size.width - Inset.toPx()
                        val (x0, x1) = if (rtl) size.width - b to size.width - a else a to b
                        drawLine(c.divider, Offset(x0, y), Offset(x1, y), 1.dp.toPx())
                    }
                },
        ) { measurables, constraints ->
            val loose = constraints.copy(minHeight = 0)
            val placeables = measurables.map { it.measure(loose) }
            val height = placeables.sumOf { it.height }
            tops.reset(placeables.size)
            layout(constraints.maxWidth, height) {
                var y = 0
                placeables.forEachIndexed { i, p ->
                    tops.ys[i] = y.toFloat()
                    val data = measurables[i].parentData as? DividerInset
                    tops.insets[i] = if (data != null) data.start else Inset
                    p.placeRelative(0, y)
                    y += p.height
                }
            }
        }
        if (footer != null) Text(
            footer,
            Modifier.padding(start = Inset, end = Inset, top = 8.dp),
            style = FootnoteStyle,
            color = c.muted,
        )
    }
}

private class FloatArrayHolder {
    var count = 0
    var ys = FloatArray(0)
    var insets = arrayOfNulls<Dp>(0)
    fun reset(n: Int) {
        count = n
        if (ys.size < n) { ys = FloatArray(n); insets = arrayOfNulls(n) }
    }
}

/** Trailing pieces a [SettingsRow] can end in. */
sealed interface Trailing {
    data object Chevron : Trailing
    data object Check : Trailing
    data object External : Trailing
}

/** A row inside a [SettingsCard]. [value] is the current choice; accent marks it as tappable. */
@Composable
fun SettingsRow(
    title: String,
    modifier: Modifier = Modifier,
    subtitle: String? = null,
    @DrawableRes icon: Int? = null,
    value: String? = null,
    valueColor: Color? = null,
    titleColor: Color? = null,
    trailing: Trailing? = null,
    enabled: Boolean = true,
    leading: (@Composable () -> Unit)? = null,
    leadingSize: Dp = Glyph,
    trailingContent: (@Composable RowScope.() -> Unit)? = null,
    onClick: (() -> Unit)? = null,
) {
    val c = Promtuz.colors
    RowFrame(
        modifier.dividerInset(if (icon != null || leading != null) Inset + leadingSize + GlyphGap else Inset)
            .then(if (onClick != null) Modifier.clickable(enabled = enabled, role = Role.Button, onClick = onClick) else Modifier),
        title, subtitle, icon, titleColor, enabled, leading,
    ) {
        value?.let { Text(it, style = RowValueStyle, color = valueColor ?: c.muted, maxLines = 1) }
        when (trailing) {
            Trailing.Chevron -> MorphIcon(MorphGlyph.ChevronRight, null, Modifier.size(20.dp), tint = c.muted)
            Trailing.Check -> DrawableIcon(com.promtuz.chat.R.drawable.oi_check, size = 20.dp, tint = c.accent)
            Trailing.External -> DrawableIcon(com.promtuz.chat.R.drawable.oi_external_link, size = 20.dp, tint = c.muted)
            null -> {}
        }
        trailingContent?.invoke(this)
    }
}

/**
 * The whole row toggles, so the label is part of the switch's touch target. With [onClick] the row
 * opens a page instead and only the switch, past a short rule, toggles.
 */
@Composable
fun SettingsSwitchRow(
    title: String,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    modifier: Modifier = Modifier,
    subtitle: String? = null,
    @DrawableRes icon: Int? = null,
    enabled: Boolean = true,
    onClick: (() -> Unit)? = null,
) {
    val inset = modifier.dividerInset(if (icon != null) Inset + Glyph + GlyphGap else Inset)
    // The row ripples like any row; a press on the switch itself is the switch's own and ripples nothing.
    if (onClick == null) RowFrame(
        inset.toggleable(checked, enabled = enabled, role = Role.Switch, onValueChange = onCheckedChange),
        title, subtitle, icon, null, enabled,
    ) { AppSwitch(checked, onCheckedChange, enabled = enabled) }
    else Row(inset.fillMaxWidth().height(IntrinsicSize.Min), verticalAlignment = Alignment.CenterVertically) {
        RowFrame(Modifier.weight(1f).clickable(enabled = enabled, role = Role.Button, onClick = onClick),
            title, subtitle, icon, null, enabled) {}
        Box(Modifier.width(1.dp).height(26.dp).background(Promtuz.colors.divider))
        Box(
            Modifier.fillMaxHeight().padding(horizontal = Inset),
            contentAlignment = Alignment.Center,
        ) { AppSwitch(checked, onCheckedChange, enabled = enabled) }
    }
}

@Composable
private fun RowFrame(
    modifier: Modifier,
    title: String,
    subtitle: String?,
    @DrawableRes icon: Int?,
    titleColor: Color?,
    enabled: Boolean,
    leading: (@Composable () -> Unit)? = null,
    trailing: @Composable RowScope.() -> Unit,
) {
    val c = Promtuz.colors
    Row(
        modifier.fillMaxWidth().heightIn(min = 52.dp).padding(horizontal = Inset, vertical = 11.dp),
        horizontalArrangement = Arrangement.spacedBy(GlyphGap),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        val a = if (enabled) 1f else 0.38f
        icon?.let { DrawableIcon(it, Modifier.alpha(a), size = Glyph, tint = c.muted) }
        leading?.invoke()
        Column(Modifier.weight(1f).alpha(a), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(title, style = RowTitleStyle, color = titleColor ?: c.text, maxLines = 2, overflow = TextOverflow.Ellipsis)
            subtitle?.let { Text(it, style = RowSubtitleStyle, color = c.muted) }
        }
        Row(Modifier.alpha(a), horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically, content = trailing)
    }
}

/** Free content inside a card (a banner, a preview), with no divider above it. */
fun Modifier.cardBlock(): Modifier = dividerInset(null)

/** A custom row whose divider starts at [start], where its text starts. */
fun Modifier.cardRow(start: Dp = Inset): Modifier = dividerInset(start)

/**
 * A row standing in for a card when the list is too long for one [SettingsCard]: row [index] of
 * [count] as a lazy item, rounding the card's ends and drawing the divider above itself.
 */
fun Modifier.lazyCardRow(index: Int, count: Int, dividerStart: Dp = Inset, divider: Boolean = index != 0): Modifier = composed {
    val c = Promtuz.colors
    clip(groupedRoundShape(index, count, major = Radii.card))
        .background(cardFill())
        .drawWithContent {
            drawContent()
            if (!divider) return@drawWithContent
            val rtl = layoutDirection == LayoutDirection.Rtl
            val y = 0.5.dp.toPx()
            val a = dividerStart.toPx(); val b = size.width - Inset.toPx()
            val (x0, x1) = if (rtl) size.width - b to size.width - a else a to b
            drawLine(c.divider, Offset(x0, y), Offset(x1, y), 1.dp.toPx())
        }
}

/** Vertical space between cards on a settings page. */
@Composable
fun CardGap() = Spacer(Modifier.height(12.dp))

data class Choice<T>(val value: T, val label: String, val description: String? = null)

/** A row whose value is one of [choices]; tapping lays the options over the row. */
@Composable
fun <T> SettingsChoiceRow(
    title: String,
    choices: List<Choice<T>>,
    selected: T,
    onSelect: (T) -> Unit,
    modifier: Modifier = Modifier,
    subtitle: String? = null,
    @DrawableRes icon: Int? = null,
    enabled: Boolean = true,
) {
    var open by androidx.compose.runtime.saveable.rememberSaveable { androidx.compose.runtime.mutableStateOf(false) }
    Box(modifier.dividerInset(if (icon != null) Inset + Glyph + GlyphGap else Inset)) {
        SettingsRow(
            title, subtitle = subtitle, icon = icon,
            value = choices.firstOrNull { it.value == selected }?.label,
            valueColor = Promtuz.colors.accent, enabled = enabled,
            onClick = { open = true },
        )
        if (open) ChoicePopup(title, choices, selected, onDismiss = { open = false }) { open = false; onSelect(it) }
    }
}

@Composable
private fun <T> ChoicePopup(
    title: String, choices: List<Choice<T>>, selected: T,
    onDismiss: () -> Unit, onPick: (T) -> Unit,
) {
    val c = Promtuz.colors
    AppModal(onDismiss, maxWidth = 328.dp) {
        Column(
            Modifier.fillMaxWidth()
                .floatingSurface(ModalShape)
                .verticalScroll(androidx.compose.foundation.rememberScrollState())
                .padding(start = 8.dp, end = 8.dp, top = 20.dp, bottom = 8.dp),
            verticalArrangement = Arrangement.spacedBy(2.dp),
        ) {
            Text(title, Modifier.padding(start = 16.dp, end = 16.dp, bottom = 10.dp).semantics { heading() },
                style = TextStyle(fontSize = 20.sp, lineHeight = 26.sp), color = c.text)
            choices.forEach { choice ->
                val on = choice.value == selected
                Column(
                    Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.tile))
                        .background(if (on) c.glassFill else Color.Transparent)
                        .clickable(role = Role.RadioButton) { onPick(choice.value) }
                        .padding(horizontal = 16.dp, vertical = 12.dp),
                    verticalArrangement = Arrangement.spacedBy(2.dp),
                ) {
                    Text(choice.label, style = RowTitleStyle.copy(fontWeight = if (on) FontWeight.Medium else FontWeight.Normal),
                        color = if (on) c.accent else c.text)
                    choice.description?.let { Text(it, style = RowSubtitleStyle, color = c.muted) }
                }
            }
        }
    }
}
