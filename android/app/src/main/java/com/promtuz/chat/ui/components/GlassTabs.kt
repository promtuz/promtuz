package com.promtuz.chat.ui.components

import androidx.compose.runtime.getValue
import androidx.annotation.DrawableRes
import androidx.compose.animation.animateColorAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.ui.theme.Radii
import dev.chrisbanes.haze.HazeState

class GlassTabItem(val label: String, @DrawableRes val icon: Int? = null)

/**
 * The floating glass switch at the foot of a composer panel; [haze] is the panel content it floats over.
 * With icons each tab stacks its icon over the label; without them the tabs are text pills.
 */
@Composable
fun GlassTabBar(haze: HazeState, tabs: List<GlassTabItem>, selected: Int, onSelect: (Int) -> Unit, modifier: Modifier = Modifier) {
    val c = Promtuz.colors
    val slab = rememberSlabState()
    // A lighter pane of the same glass, never a solid chip sitting on it.
    Row(
        modifier
            .navigationBarsPadding()
            .padding(bottom = 6.dp)
            .floatingSurface(RoundedCornerShape(Radii.pill), haze)
            .slabHost(slab, selected, tabs.size, c.glassFill, concentric(Radii.pill, 4.dp), onSelect)
            .padding(4.dp),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        tabs.forEachIndexed { i, tab ->
            val tint by animateColorAsState(if (slab.shown(selected) == i) c.accent else c.text, label = "tabTint")
            val option = Modifier.slabOption(slab, i, i == selected, Role.Tab) { onSelect(i) }
            if (tab.icon != null) Column(
                option.size(80.dp, 48.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(2.dp, Alignment.CenterVertically),
            ) {
                DrawableIcon(tab.icon, tint = tint, size = 24.dp)
                Text(tab.label, style = IconTabLabel, color = tint)
            } else Box(
                option.height(36.dp).widthIn(min = 72.dp).padding(horizontal = 16.dp),
                contentAlignment = Alignment.Center,
            ) {
                Text(tab.label, style = TextTabLabel, color = tint)
            }
        }
    }
}

private val IconTabLabel = TextStyle(fontSize = 11.sp, lineHeight = 14.sp, fontWeight = FontWeight.Medium)
private val TextTabLabel = TextStyle(fontSize = 14.sp, lineHeight = 18.sp, fontWeight = FontWeight.Medium)
