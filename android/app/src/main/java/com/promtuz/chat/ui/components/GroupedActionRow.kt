package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.text.avgSizeInStyle
import com.promtuz.chat.ui.util.groupedRoundShape

/** Shared Settings-style action row. Place siblings 4.dp apart in a group. */
@Composable
fun GroupedActionRow(
    title: String,
    index: Int,
    groupSize: Int,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    icon: @Composable () -> Unit,
) {
    val colors = MaterialTheme.colorScheme
    val typography = MaterialTheme.typography
    Row(modifier.fillMaxWidth()
        .clip(groupedRoundShape(index, groupSize))
        .background(colors.surfaceContainerLow)
        .clickable(enabled = enabled, role = Role.Button, onClick = onClick)
        .padding(vertical = 12.dp, horizontal = 16.dp),
        horizontalArrangement = Arrangement.spacedBy(20.dp),
        verticalAlignment = Alignment.CenterVertically) {
        CompositionLocalProvider(LocalContentColor provides colors.onSurface) {
            Box(Modifier.alpha(if (enabled) 1f else 0.38f)) { icon() }
        }
        Text(title, modifier = Modifier.weight(1f).alpha(if (enabled) 1f else 0.38f),
            style = avgSizeInStyle(typography.labelLargeEmphasized, typography.bodyLargeEmphasized, 0.75f),
            color = colors.onBackground)
    }
}
