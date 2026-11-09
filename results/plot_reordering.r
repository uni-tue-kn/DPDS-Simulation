#!/usr/bin/env Rscript

library(ggplot2)
library(magrittr)
library(dplyr)
library(scales)
library(tidyr)
library(patchwork)

# Define the parameters
delays <- c("10ms")
jitters <- c("100us", "300us", "1000us", "3000us")
rates <- c("0.1Gbps", "1Gbps", "10Gbps", "100Gbps")
distributions <- c("normal")

# Plot parameters
font_size <- 34.9
line_size <- 2.2
point_size <- 7.0
conf_alpha <- 0.05
geom_errorbar_width <- 0.3

##################################################################################################
# Data loading

# Read every run for a given on-disk filename prefix and tag it with a plot prefix.
# file_prefix: filename stem on disk (e.g. "reorder_normal", "reorder_kendalltau_normal").
# plot_prefix: logical configuration label used for grouping/colors ("reorder", "correlation30").
read_dataset <- function(file_prefix, plot_prefix) {
    df <- data.frame()
    for (distribution in distributions) {
        for (delay in delays) {
            for (jitter in jitters) {
                for (rate in rates) {
                    filename <- paste0(file_prefix, "_", distribution, "_", delay, "_", jitter, "_", rate, ".csv")

                    if (file.exists(filename)) {
                        data <- read.csv(filename)
                        data$prefix <- plot_prefix
                        data$distribution <- distribution
                        data$delay_config <- delay
                        data$jitter_config <- jitter
                        data$rate_config <- rate
                        data$rate_numeric <- as.numeric(gsub("Gbps", "", rate))
                        df <- rbind(df, data)
                    } else {
                        warning(paste("File not found:", filename))
                    }
                }
            }
        }
    }
    df
}

# Out-of-order packets: use the old CSVs (OutOfOrderCounter metric).
# read_dataset appends the distribution, so the prefixes here omit it.
df_ooo_raw <- rbind(
    read_dataset("reorder", "reorder"),
    read_dataset("correlation30", "correlation30")
)

# Kendall tau distance: use the new CSVs (KendallTauDistance metric). Only the reorder
# configuration has new CSVs; correlation reordering is always zero, so we derive the
# correlated rows from the reorder structure with the metric forced to 0.
df_ktau_reorder <- read_dataset("reorder_kendalltau", "reorder")
df_ktau_corr <- df_ktau_reorder %>%
    mutate(
        prefix = "correlation30",
        kendall_tau_distance_div_n = 0
    )
df_ktau_raw <- rbind(df_ktau_reorder, df_ktau_corr)

##################################################################################################
# Aggregation

# Aggregate a single metric across runs and attach a confidence-interval half-width.
# Returns columns: prefix, distribution, delay_config, jitter_config, rate_config,
# rate_numeric, avg, sd, n_runs, ci.
aggregate_metric <- function(df_raw, metric) {
    df_raw$jitter_config <- factor(df_raw$jitter_config,
        levels = c("100us", "300us", "1000us", "3000us"),
        labels = c("0.1 ms", "0.3 ms", "1 ms", "3 ms")
    )

    df_raw %>%
        group_by(prefix, distribution, delay_config, jitter_config, rate_config, rate_numeric) %>%
        summarise(
            avg = mean(.data[[metric]]),
            sd = sd(.data[[metric]]),
            n_runs = n(),
            .groups = "drop"
        ) %>%
        mutate(ci = qt(1 - conf_alpha / 2, n_runs - 1) * (sd / sqrt(n_runs)))
}

df_ooo <- aggregate_metric(df_ooo_raw, "out_of_order_packets")

# The new Kendall tau CSVs may not exist yet; skip that plot if none were found.
have_ktau <- nrow(df_ktau_raw) > 0
if (have_ktau) {
    df_ktau <- aggregate_metric(df_ktau_raw, "kendall_tau_distance_div_n")
} else {
    warning("No reorder_kendalltau_* CSVs found; skipping the Kendall tau distance plot.")
}

##################################################################################################
# Shared plotting setup

# Convert prefix and rate to ordered factors and compute x positions with group spacing.
finalize_factors <- function(df) {
    df$prefix <- factor(df$prefix,
        levels = c("basic", "reorder", "correlation30"),
        labels = c("Uncorrelated", "Reordered", "Correlated")
    )
    df$rate_config <- factor(df$rate_config,
        levels = c("0.1Gbps", "1Gbps", "10Gbps", "100Gbps"),
        labels = c("0.1", "1", "10", "100")
    )
    df$rate_x <- rate_positions[as.integer(df$rate_config)]
    df
}

rate_levels <- c("0.1", "1", "10", "100")
group_spacing <- 1.25
rate_positions <- 1 + (seq_along(rate_levels) - 1) * group_spacing
group_dividers <- rate_positions[-1] - group_spacing / 2

dodge <- position_dodge(width = 0.7)

base_theme <- theme_minimal() +
    theme(
        panel.grid.minor = element_blank(),
        panel.grid.major = element_blank(),
        axis.text = element_text(size = font_size),
        axis.title = element_text(size = font_size, face = "bold"),
        axis.line = element_line(),
        axis.ticks = element_line(),
        legend.position = "bottom",
        legend.location = "plot",
        legend.text = element_text(size = font_size, margin = margin(l = 8)),
        legend.title = element_text(size = font_size, face = "italic", margin = margin(r = 8)),
        legend.key.spacing.x = unit(10, "pt"),
        plot.title = element_text(size = font_size, face = "bold", hjust = 0.5),
        plot.margin = margin(t = 25, r = 10, b = 5, l = 5)
    )

legend_theme <- theme(legend.position = "bottom", legend.box = "vertical")

df_ooo <- finalize_factors(df_ooo)
if (have_ktau) {
    df_ktau <- finalize_factors(df_ktau)
}

##################################################################################################
# Plots

for (dist in distributions) {
    df_ooo_dist <- df_ooo %>% filter(distribution == dist)

    # Plot 1: Out-of-order packets (old CSVs, Reordered + Correlated)
    plot_ooo <- ggplot(df_ooo_dist, aes(
        x = rate_x,
        y = pmax(avg, 1),
        color = prefix,
        shape = jitter_config,
        group = interaction(jitter_config, prefix)
    )) +
        geom_vline(xintercept = group_dividers, linetype = "dashed", color = "grey50") +
        geom_errorbar(
            aes(
                ymin = pmax(avg - ci, 1),
                ymax = pmax(avg + ci, 1)
            ),
            width = geom_errorbar_width,
            linetype = "solid",
            linewidth = line_size,
            position = dodge
        ) +
        geom_point(size = point_size, position = dodge) +
        scale_x_continuous(breaks = rate_positions, labels = rate_levels) +
        scale_y_log10(
            breaks = c(1, 1e3, 1e6, 1e9),
            labels = function(x) ifelse(x == 1, "0", label_number(big.mark = " ")(x))
        ) +
        scale_color_manual(values = c(Uncorrelated = "#000000", Reordered = "#000000", Correlated = "#0072B2")) +
        scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
        guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
        base_theme +
        labs(
            x = "Rate (Gbit/s)",
            y = "Out-of-order\npackets",
            color = "Configuration",
            shape = "Jitter"
        )

    ggsave(
        file = paste0("combined_ooo_", dist, "_reordered_correlated.pdf"),
        plot = plot_ooo + legend_theme,
        device = cairo_pdf,
        width = 14.4, height = 6
    )
    cat(paste0("\nPlot saved as: combined_ooo_", dist, "_reordered_correlated.pdf\n"))

    if (!have_ktau) {
        next
    }

    df_ktau_dist <- df_ktau %>% filter(distribution == dist)

    # Plot 2: Kendall tau distance (new CSVs, Reordered + Correlated; correlated = 0)
    plot_ktau <- ggplot(df_ktau_dist, aes(
        x = rate_x,
        y = pmax(avg, 1),
        color = prefix,
        shape = jitter_config,
        group = interaction(jitter_config, prefix)
    )) +
        geom_vline(xintercept = group_dividers, linetype = "dashed", color = "grey50") +
        geom_errorbar(
            aes(
                ymin = pmax(avg - ci, 1),
                ymax = pmax(avg + ci, 1)
            ),
            width = geom_errorbar_width,
            linetype = "solid",
            linewidth = line_size,
            position = dodge
        ) +
        geom_point(size = point_size, position = dodge) +
        scale_x_continuous(breaks = rate_positions, labels = rate_levels) +
        scale_y_log10(
            breaks = c(1, 1e1, 1e2, 1e3, 1e4),
            labels = function(x) ifelse(x == 1, "0", label_number(big.mark = " ")(x))
        ) +
        scale_color_manual(values = c(Uncorrelated = "#000000", Reordered = "#000000", Correlated = "#0072B2")) +
        scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
        guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
        base_theme +
        labs(
            x = "Rate (Gbit/s)",
            y = "Swaps per\npacket",
            color = "Configuration",
            shape = "Jitter"
        )

    ggsave(
        file = paste0("combined_ktau_", dist, "_reordered_correlated.pdf"),
        plot = plot_ktau + legend_theme,
        device = cairo_pdf,
        width = 14.4, height = 6
    )
    cat(paste0("\nPlot saved as: combined_ktau_", dist, "_reordered_correlated.pdf\n"))
}
