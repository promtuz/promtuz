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
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.text.avgSizeInStyle
import com.promtuz.chat.ui.util.groupedRoundShape

/**
 * Place siblings 4.dp apart in a group. A row with a trailing [control] is one semantic control
 * and presses by scaling, so the label and the control share the feedback.
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
    checked: Boolean? = null,
    selected: Boolean? = null,
    control: (@Composable () -> Unit)? = null,
    icon: (@Composable () -> Unit)? = null,
) {
    val colors = MaterialTheme.colorScheme
    val typography = MaterialTheme.typography
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
    Row(modifier.fillMaxWidth()
        .graphicsLayer { scaleX = scale; scaleY = scale }
        .clip(groupedRoundShape(index, groupSize))
        .background(colors.surfaceContainerLow)
        .then(action)
        .padding(vertical = 12.dp, horizontal = 16.dp),
        horizontalArrangement = Arrangement.spacedBy(16.dp),
        verticalAlignment = Alignment.CenterVertically) {
        if (icon != null) CompositionLocalProvider(LocalContentColor provides colors.onSurface) {
            Box(Modifier.padding(end = 4.dp).alpha(alpha)) { icon() }
        }
        Column(Modifier.weight(1f).alpha(alpha), verticalArrangement = Arrangement.spacedBy(3.dp)) {
            Text(title,
                style = avgSizeInStyle(typography.labelLargeEmphasized, typography.bodyLargeEmphasized, 0.75f),
                color = colors.onBackground)
            supportingText?.let {
                Text(it, style = typography.bodyMedium, color = colors.onSurfaceVariant)
            }
        }
        control?.invoke()
    }
}

@Composable
fun SettingsSection(title: String, top: Dp = 20.dp) {
    Text(
        title.uppercase(),
        Modifier.fillMaxWidth().padding(top = top, bottom = 3.dp, start = 2.dp).semantics { heading() },
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = avgSizeInStyle(MaterialTheme.typography.labelLargeEmphasized, MaterialTheme.typography.labelMediumEmphasized),
    )
}
