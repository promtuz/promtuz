package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.promtuz.chat.presentation.viewmodel.GroupVM
import com.promtuz.chat.presentation.viewmodel.GroupWork
import com.promtuz.chat.ui.appearance.LocalChatColors
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.SimpleScreen
import org.koin.androidx.compose.koinViewModel

/**
 * Name a group and pick who's in it.
 *
 * Creation is a real network round trip — every member's KeyPackage has to be
 * fetched and Welcomed — so the button blocks and failures surface here rather
 * than leaving a half-built group behind.
 */
@Composable
fun NewGroupScreen(viewModel: GroupVM = koinViewModel()) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    val title by viewModel.title.collectAsStateWithLifecycle()
    val picked by viewModel.picked.collectAsStateWithLifecycle()
    val candidates by viewModel.candidates.collectAsStateWithLifecycle()
    val work by viewModel.work.collectAsStateWithLifecycle()

    SimpleScreen({ Text("New group") }) { padding ->
    Column(Modifier.fillMaxSize().padding(padding)) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 16.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Avatar(name = title.ifBlank { "G" }, size = 52.dp)
            Spacer(Modifier.width(14.dp))
            Column(Modifier.weight(1f)) {
                BasicTextField(
                    value = title,
                    onValueChange = viewModel::setTitle,
                    singleLine = true,
                    textStyle = MaterialTheme.typography.titleMedium.copy(color = colors.onSurface),
                    cursorBrush = SolidColor(chat.accent),
                    decorationBox = { inner ->
                        if (title.isEmpty()) {
                            Text(
                                "Group name",
                                style = MaterialTheme.typography.titleMedium,
                                color = colors.onSurfaceVariant.copy(alpha = 0.6f),
                            )
                        }
                        inner()
                    },
                )
                Text(
                    if (picked.isEmpty()) "No one selected yet"
                    else "${picked.size} selected",
                    style = MaterialTheme.typography.bodySmall,
                    color = colors.onSurfaceVariant,
                )
            }
        }

        (work as? GroupWork.Failed)?.let {
            Text(
                it.reason,
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 20.dp, vertical = 8.dp),
                style = MaterialTheme.typography.bodySmall,
                color = colors.error,
            )
        }

        LazyColumn(Modifier.weight(1f)) {
            items(candidates, key = { it.ipkHex }) { c ->
                MemberPickRow(
                    name = c.name,
                    selected = c.ipkHex in picked,
                    onClick = { viewModel.togglePick(c.ipkHex) },
                )
            }
            item { Spacer(Modifier.height(96.dp)) }
        }

        Box(Modifier.fillMaxWidth().padding(20.dp)) {
            val enabled = picked.isNotEmpty() && work !is GroupWork.Busy
            Row(
                Modifier
                    .fillMaxWidth()
                    .clip(MaterialTheme.shapes.large)
                    .background(if (enabled) chat.accent else colors.surfaceContainerHigh)
                    .clickable(enabled = enabled) { viewModel.create() }
                    .padding(vertical = 14.dp),
                horizontalArrangement = Arrangement.Center,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                if (work is GroupWork.Busy) {
                    CircularProgressIndicator(
                        Modifier.size(18.dp),
                        strokeWidth = 2.dp,
                        color = colors.onSurface,
                    )
                } else {
                    Text(
                        "Create group",
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                        color = if (enabled) Color.White else colors.onSurfaceVariant,
                    )
                }
            }
        }
    }
    }
}

/** One selectable contact; the tick is the whole affordance. */
@Composable
private fun MemberPickRow(name: String, selected: Boolean, onClick: () -> Unit) {
    val colors = MaterialTheme.colorScheme
    val chat = LocalChatColors.current
    Row(
        Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 20.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Avatar(name = name, size = 42.dp)
        Spacer(Modifier.width(14.dp))
        Text(
            name,
            Modifier.weight(1f),
            style = MaterialTheme.typography.bodyLarge,
            color = colors.onSurface,
        )
        Box(
            Modifier
                .size(22.dp)
                .clip(CircleShape)
                .background(if (selected) chat.accent else colors.surfaceContainerHigh),
            contentAlignment = Alignment.Center,
        ) {
            if (selected) {
                Text("✓", style = MaterialTheme.typography.labelMedium, color = Color.White)
            }
        }
    }
}
