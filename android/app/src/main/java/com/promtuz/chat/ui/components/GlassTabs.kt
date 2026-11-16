package com.promtuz.chat.ui.components

import androidx.annotation.DrawableRes
import androidx.compose.animation.animateColorAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii
import dev.chrisbanes.haze.HazeState

/** The floating glass switch at the foot of a composer panel; [haze] is the panel content it floats over. */
@Composable
fun GlassTabBar(haze: HazeState, modifier: Modifier = Modifier, content: @Composable RowScope.() -> Unit) {
    Row(
        modifier
            .navigationBarsPadding()
            .padding(bottom = 6.dp)
            .floatingSurface(RoundedCornerShape(Radii.pill), haze)
            .padding(4.dp),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) { content() }
}

/** With an [icon] the tab stacks it over the label; without one it is a text pill. */
@Composable
fun GlassTab(label: String, @DrawableRes icon: Int?, selected: Boolean, onClick: () -> Unit) {
    val c = Promtuz.colors
    // A lighter pane of the same glass, never a solid chip sitting on it.
    val tint by animateColorAsState(if (selected) c.accent else c.text, label = "tabTint")
    val fill by animateColorAsState(if (selected) c.glassFill else Color.Transparent, label = "tabFill")
    val shape = RoundedCornerShape(concentric(Radii.pill, 4.dp))
    if (icon != null) Column(
        Modifier.size(80.dp, 48.dp).clip(shape).background(fill)
            .clickable(role = Role.Tab, onClick = onClick).semantics { this.selected = selected },
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(2.dp, Alignment.CenterVertically),
    ) {
        DrawableIcon(icon, tint = tint, size = 24.dp)
        Text(label, style = IconTabLabel, color = tint)
    } else Box(
        Modifier.height(36.dp).widthIn(min = 72.dp).clip(shape).background(fill)
            .clickable(role = Role.Tab, onClick = onClick).semantics { this.selected = selected }
            .padding(horizontal = 16.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, style = TextTabLabel, color = tint)
    }
}

private val IconTabLabel = TextStyle(fontSize = 11.sp, lineHeight = 14.sp, fontWeight = FontWeight.Medium)
private val TextTabLabel = TextStyle(fontSize = 14.sp, lineHeight = 18.sp, fontWeight = FontWeight.Medium)
