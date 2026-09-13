package com.promtuz.chat.ui.components

import androidx.compose.animation.*
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.res.painterResource
import com.promtuz.chat.R
import com.promtuz.chat.ui.stage.ChatMotion
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.Dp
import com.promtuz.chat.presentation.viewmodel.GroupWork
import com.promtuz.chat.presentation.viewmodel.UiMember

/** Shared contact rows for starting chats, creating groups and adding members. */
@Composable
fun ContactPicker(
    modifier: Modifier = Modifier,
    people: List<UiMember>,
    query: String,
    selected: Set<String>,
    selecting: Boolean,
    enabled: Boolean,
    onClick: (UiMember) -> Unit,
    onLongClick: (UiMember) -> Unit = onClick,
    emptyText: String = "No contacts yet",
    yPadding: Pair<Dp, Dp> = Pair(16.dp, 16.dp),
    supportingText: @Composable (UiMember) -> Unit = {},
    header: @Composable () -> Unit = {},
) {
    val (topPadding, bottomPadding) = yPadding
    val visible = people.filter { it.name.contains(query.trim(), ignoreCase = true) }
    Column(modifier) {
        LazyColumn(Modifier.weight(1f), contentPadding = PaddingValues(top = topPadding, bottom = bottomPadding)) {
            item {
                AnimatedVisibility(selecting && selected.isNotEmpty(),
                    enter = fadeIn(ChatMotion.spec()) + expandVertically(ChatMotion.spec()),
                    exit = fadeOut(ChatMotion.spec()) + shrinkVertically(ChatMotion.spec())) {
                    LazyRow(contentPadding = PaddingValues(horizontal = 16.dp, vertical = 8.dp),
                        horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        items(people.filter { it.ipkHex in selected }, key = { it.ipkHex }) { person ->
                            Row(Modifier.animateItem().clip(CircleShape)
                                .background(MaterialTheme.colorScheme.primary.copy(alpha = 0.12f))
                                .clickable(enabled = enabled) { onClick(person) }
                                .padding(start = 12.dp, end = 8.dp, top = 8.dp, bottom = 8.dp),
                                verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                Text(person.name, maxLines = 1, style = MaterialTheme.typography.labelLarge)
                                Icon(painterResource(R.drawable.i_close), "Deselect ${person.name}", Modifier.size(12.dp))
                            }
                        }
                    }
                }
            }

            item {
                header()
            }

            if (visible.isEmpty()) item {
                Text(if (query.isBlank()) emptyText else "No contacts found",
                    Modifier.padding(24.dp), color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            items(visible, key = { it.ipkHex }) { person ->
                Row(
                    Modifier.animateItem(placementSpec = null).fillMaxWidth()
                        .semantics { this.selected = person.ipkHex in selected }
                        .combinedClickable(enabled = enabled, role = if (selecting) Role.Checkbox else Role.Button,
                            onClick = { onClick(person) }, onLongClick = { onLongClick(person) })
                        .padding(horizontal = 20.dp, vertical = 7.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(14.dp),
                ) {
                    SelectableContactAvatar(person.name, selecting, person.ipkHex in selected)
                    Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                        Text(person.name, style = MaterialTheme.typography.bodyLarge,
                            maxLines = 1, overflow = TextOverflow.Ellipsis)
                        supportingText(person)
                    }
                }
            }
        }
    }
}

@Composable
private fun SelectableContactAvatar(name: String, selecting: Boolean, selected: Boolean) {
    val colors = MaterialTheme.colorScheme
    val scale by animateFloatAsState(if (selected) 0.94f else 1f, spring(), label = "selected avatar")
    val check by animateFloatAsState(if (selected) 1f else 0f, spring(), label = "contact check")
    val fill by animateColorAsState(if (selected) colors.primary else colors.surfaceContainerHigh,
        ChatMotion.spec(), label = "contact selection fill")
    Box(Modifier.size(48.dp)) {
        Box(Modifier.align(Alignment.Center).graphicsLayer { scaleX = scale; scaleY = scale }) { Avatar(name, size = 44.dp) }
        AnimatedVisibility(selected, Modifier.align(Alignment.BottomEnd),
            enter = fadeIn(ChatMotion.spec()) + scaleIn(spring(), initialScale = 0.4f),
            exit = fadeOut(ChatMotion.spec()) + scaleOut(ChatMotion.spec(), targetScale = 0.4f)) {
            Box(Modifier.size(20.dp).clip(CircleShape).background(fill).border(2.dp, colors.background, CircleShape),
                contentAlignment = Alignment.Center) {
                Icon(painterResource(R.drawable.i_check), null, Modifier.size(10.dp).graphicsLayer {
                    alpha = check; scaleX = check; scaleY = check
                }, tint = colors.onPrimary)
            }
        }
    }
}

@Composable
fun GroupWorkFeedback(work: GroupWork) {
    when (work) {
        is GroupWork.Busy -> Row(Modifier.fillMaxWidth().padding(16.dp),
            verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
            Text(work.label, style = MaterialTheme.typography.bodyMedium)
        }
        is GroupWork.Failed -> Text(work.reason, Modifier.padding(16.dp), color = MaterialTheme.colorScheme.error)
        GroupWork.Idle -> Unit
    }
}
