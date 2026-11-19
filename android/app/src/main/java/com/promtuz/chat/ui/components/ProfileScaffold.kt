package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Promtuz

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.ui.draw.clip
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.BiasAlignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.lerp
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.lerp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.text.ttCommonsProFamily
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState

private val ProfileNameStyle = TextStyle(fontFamily = ttCommonsProFamily, fontWeight = FontWeight.SemiBold, fontSize = 26.sp, lineHeight = 32.sp)
private val ProfileStatusStyle = TextStyle(fontSize = 15.sp, lineHeight = 20.sp)
private val ProfileStatusSmall = TextStyle(fontSize = 13.sp, lineHeight = 16.sp)

/** One photo and name move from the expanded profile into the standard Back/overflow toolbar. */
@Composable
fun ProfileScaffold(
    name: String,
    photo: @Composable (Dp) -> Unit,
    subtitle: String? = null,
    subtitleColor: Color = Promtuz.colors.muted,
    @androidx.annotation.DrawableRes subtitleIcon: Int? = null,
    actions: @Composable RowScope.() -> Unit = {},
    snackbarHost: @Composable () -> Unit = {},
    content: @Composable (PaddingValues, LazyListState) -> Unit,
) {
    val c = Promtuz.colors
    val expanded = if (subtitle != null) 210.dp else 186.dp
    val list = rememberLazyListState()
    // Content that fits never collapses the header; once collapsed it can always be pulled back open.
    lateinit var scroll: TopAppBarScrollBehavior
    scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior(
        canScroll = { list.canScrollForward || list.canScrollBackward || scroll.state.heightOffset < 0f },
    )
    val range = with(LocalDensity.current) { (expanded - 64.dp).toPx() }
    SideEffect { scroll.state.heightOffsetLimit = -range }
    val haze = rememberHazeState()
    Scaffold(
        modifier = Modifier.fillMaxSize().nestedScroll(scroll.nestedScrollConnection),
        containerColor = c.groupedPage,
        snackbarHost = snackbarHost,
        topBar = {
            val progress = scroll.state.collapsedFraction
            BoxWithConstraints(
                Modifier.fillMaxWidth()
                    .frost(haze, FrostRole.Bar, page = c.groupedPage, scrolled = { scroll.state.overlappedFraction })
                    .windowInsetsPadding(TopAppBarDefaults.windowInsets)
                    .height(lerp(expanded, 64.dp, progress)),
            ) {
                val photoSize = lerp(112.dp, 36.dp, progress)
                Box(Modifier.offset(x = lerp((maxWidth - photoSize) / 2, 56.dp, progress),
                    y = lerp(16.dp, 14.dp, progress))) { photo(photoSize) }
                // Name and subtitle travel together into the toolbar, like a chat's title and status.
                val textX = lerp(16.dp, 104.dp, progress)
                val textW = lerp(maxWidth - 32.dp, (maxWidth - 160.dp).coerceAtLeast(0.dp), progress)
                val align = BiasAlignment(-progress, 0f)
                val collapsedName = if (subtitle != null) screenTitleStyle().copy(fontSize = 19.sp, lineHeight = 22.sp) else screenTitleStyle()
                Box(
                    Modifier.offset(x = textX, y = lerp(142.dp, if (subtitle != null) 6.dp else 14.dp, progress))
                        .width(textW).height(if (subtitle != null) lerp(36.dp, 28.dp, progress) else 36.dp),
                    contentAlignment = align,
                ) {
                    Text(name, style = lerp(ProfileNameStyle, collapsedName, progress), color = c.text,
                        maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
                if (subtitle != null) Box(
                    Modifier.offset(x = textX, y = lerp(180.dp, 34.dp, progress)).width(textW).height(20.dp),
                    contentAlignment = align,
                ) {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                        subtitleIcon?.let { DrawableIcon(it, size = 15.dp, tint = subtitleColor) }
                        Text(subtitle, style = lerp(ProfileStatusStyle, ProfileStatusSmall, progress), color = subtitleColor,
                            maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                }
                // Keep the same controls and hit targets throughout the collapse.
                TopAppBar(
                    title = {}, navigationIcon = { GoBackButton() }, actions = actions,
                    windowInsets = WindowInsets(0),
                    colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
                )
            }
        },
    ) { padding ->
        Box(Modifier.fillMaxSize().background(c.groupedPage).hazeSource(haze)) { content(padding, list) }
    }
}

/** Equal tiles under a profile header: the primary things to do with this person or group. */
@Composable
fun ProfileActions(actions: List<MenuAction>, modifier: Modifier = Modifier) {
    val c = Promtuz.colors
    Row(modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        actions.forEach { action ->
            Column(
                Modifier.weight(1f).height(68.dp)
                    .clip(androidx.compose.foundation.shape.RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.tile))
                    .background(c.card)
                    .clickable(onClick = action.onClick),
                horizontalAlignment = androidx.compose.ui.Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(6.dp, androidx.compose.ui.Alignment.CenterVertically),
            ) {
                action.icon?.let { DrawableIcon(it, size = 22.dp, tint = c.text) }
                Text(action.label, style = TextStyle(fontSize = 13.sp, lineHeight = 16.sp, fontWeight = FontWeight.Medium),
                    color = c.text, maxLines = 1)
            }
        }
    }
}
