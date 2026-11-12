package com.promtuz.chat.ui.components

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.LocalIndication
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.toggleable
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.util.groupedRoundShape

val RowTitleStyle = TextStyle(fontSize = 16.sp, lineHeight = 22.sp)
val RowSubtitleStyle = TextStyle(fontSize = 13.5.sp, lineHeight = 18.sp)
val RowValueStyle = TextStyle(fontSize = 15.sp, lineHeight = 20.sp)

private val RowGlyph = 24.dp
private val RowGap = 18.dp
private val RowInset = 16.dp

/**
 * One row of a card: siblings sit flush and the card's divider runs under every row but the last,
 * starting at the text. A row with a trailing [control] is one semantic control and presses by
 * scaling, so the label and the control share the feedback.
 */
@Composable
fun GroupedActionRow(
    title: String,
    index: Int,
    groupSize: Int,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    supportingText: String? = null,
    value: String? = null,
    titleColor: androidx.compose.ui.graphics.Color? = null,
    checked: Boolean? = null,
    selected: Boolean? = null,
    control: (@Composable () -> Unit)? = null,
    icon: (@Composable () -> Unit)? = null,
) {
    val c = Promtuz.colors
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(if (pressed && control != null) 0.98f else 1f, spring(stiffness = 900f), label = "settingPress")
    val indication = if (control == null) LocalIndication.current else null
    val action = when {
        checked != null -> Modifier.toggleable(checked, interaction, indication, enabled, Role.Switch) { onClick() }
        selected != null -> Modifier.selectable(selected, interaction, indication, enabled, Role.RadioButton, onClick)
        else -> Modifier.clickable(interaction, indication, enabled, role = Role.Button, onClick = onClick)
    }
    val alpha = if (enabled) 1f else 0.38f
    val last = index == groupSize - 1
    val rtl = LocalLayoutDirection.current == LayoutDirection.Rtl
    val dividerStart = if (icon != null) RowInset + RowGlyph + RowGap else RowInset
    Row(modifier.fillMaxWidth()
        .graphicsLayer { scaleX = scale; scaleY = scale }
        .clip(groupedRoundShape(index, groupSize, major = 20, minor = 0))
        .background(c.card)
        .then(action)
        .drawBehind {
            if (last) return@drawBehind
            val y = size.height - 0.5.dp.toPx()
            val a = dividerStart.toPx(); val b = size.width - RowInset.toPx()
            val (x0, x1) = if (rtl) size.width - b to size.width - a else a to b
            drawLine(c.divider, Offset(x0, y), Offset(x1, y), 1.dp.toPx())
        }
        .heightIn(min = 52.dp)
        .padding(vertical = 11.dp, horizontal = RowInset),
        horizontalArrangement = Arrangement.spacedBy(RowGap),
        verticalAlignment = Alignment.CenterVertically) {
        if (icon != null) CompositionLocalProvider(LocalContentColor provides c.muted) {
            Box(Modifier.size(RowGlyph).alpha(alpha), contentAlignment = Alignment.Center) { icon() }
        }
        Column(Modifier.weight(1f).alpha(alpha), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(title, style = RowTitleStyle, color = titleColor ?: c.text)
            supportingText?.let { Text(it, style = RowSubtitleStyle, color = c.muted) }
        }
        value?.let { Text(it, Modifier.alpha(alpha), style = RowValueStyle, color = c.muted) }
        control?.invoke()
    }
}

@Composable
fun SettingsSection(title: String, top: Dp = 20.dp) {
    Text(
        title,
        Modifier.fillMaxWidth().padding(top = top, bottom = 4.dp, start = 16.dp, end = 16.dp).semantics { heading() },
        color = Promtuz.colors.accent,
        style = TextStyle(fontSize = 14.sp, lineHeight = 20.sp, fontWeight = FontWeight.Medium),
    )
}
