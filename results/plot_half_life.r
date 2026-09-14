#!/usr/bin/env Rscript

library(ggplot2)
library(magrittr)
library(dplyr)
library(scales)
library(tidyr)
library(patchwork)

# Define the parameters
# Memory values M used in the CSV filenames (correlation<M>_...)
memories <- c(10, 30, 100)
# Corresponding half-life periods t_h = M * ln(2), keyed by memory M
half_life_map <- c("10" = 7, "30" = 21, "100" = 70)
half_lives <- unname(half_life_map[as.character(memories)])
delays <- c("10ms")
jitters <- c("100us", "300us", "1000us", "3000us")
rates <- c("0.1Gbps", "1Gbps", "10Gbps", "100Gbps")
distributions <- c("normal")

# Plot parameters
font_size <- 34.9
line_size <- 2.2
point_size <- 5.3
conf_alpha <- 0.05
geom_errorbar_width <- 0.5

# Colors and linetypes for each half-life period
half_life_colors <- c(
    "7 ms"  = "#000000",
    "21 ms" = "#0072B2",
    "70 ms" = "#D55E00"
)

# Initialize an empty dataframe to store all results
df <- data.frame(
    half_life = integer(),
    distribution = character(),
    delay_config = character(),
    jitter_config = character(),
    rate_config = character(),
    rate_numeric = numeric(),
    delay_mean_ns = numeric(),
    delay_std_ns = numeric(),
    inter_arrival_time_mean_ns = numeric(),
    inter_arrival_time_std_ns = numeric(),
    tracked_packets = numeric()
)

# Read all CSV files for each half_life (filenames encode the half_life t_h)
for (memory in memories) {
    prefix <- paste0("correlation", memory)
    for (distribution in distributions) {
        for (delay in delays) {
            for (jitter in jitters) {
                for (rate in rates) {
                    filename <- paste0(prefix, "_", distribution, "_", delay, "_", jitter, "_", rate, ".csv")

                    if (file.exists(filename)) {
                        data <- read.csv(filename)
                        data$half_life <- half_life_map[as.character(memory)]
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
}

##################################################################################################
# Sanity checks: warn if any packets were dropped

dropped_bw <- df %>% filter(dropped_packets_bandwidth > 0)
if (nrow(dropped_bw) > 0) {
    warning(paste0(
        nrow(dropped_bw), " run(s) have dropped_packets_bandwidth > 0 ",
        "(total dropped: ", sum(dropped_bw$dropped_packets_bandwidth), ")"
    ))
}

dropped_bvq <- df %>% filter(dropped_packets_burst_virtual_queue > 0)
if (nrow(dropped_bvq) > 0) {
    warning(paste0(
        nrow(dropped_bvq), " run(s) have dropped_packets_burst_virtual_queue > 0 ",
        "(total dropped: ", sum(dropped_bvq$dropped_packets_burst_virtual_queue), ")"
    ))
}

# Factor jitter for ordered legend and display
df$jitter_config <- factor(df$jitter_config,
    levels = c("100us", "300us", "1000us", "3000us"),
    labels = c("0.1 ms", "0.3 ms", "1 ms", "3 ms")
)

# Factor half-life for ordered legend
df$half_life <- factor(df$half_life, levels = half_lives, labels = paste(half_lives, "ms"))

# Calculate per-run deviation percentages: (measured - configured) / configured * 100
df <- df %>%
    mutate(
        configured_delay_ns = as.numeric(gsub("ms", "", delay_config)) * 1e6,
        configured_jitter_ns = as.numeric(gsub("ms", "", as.character(jitter_config))) * 1e6,
        delay_deviation_pct = (delay_mean_ns - configured_delay_ns) / configured_delay_ns * 100,
        jitter_deviation_pct = (delay_std_ns - configured_jitter_ns) / configured_jitter_ns * 100
    )

# Aggregate statistics across runs
df_aggregated <- df %>%
    group_by(half_life, distribution, jitter_config, rate_config, rate_numeric) %>%
    summarise(
        delay_deviation_pct_avg = mean(delay_deviation_pct),
        delay_deviation_pct_sd = sd(delay_deviation_pct),
        jitter_deviation_pct_avg = mean(jitter_deviation_pct),
        jitter_deviation_pct_sd = sd(jitter_deviation_pct),
        theoretical_passage_time_mean_ns_avg = mean(theoretical_passage_time_mean_ns),
        theoretical_passage_time_mean_ns_sd = sd(theoretical_passage_time_mean_ns),
        n_runs = n(),
        .groups = "drop"
    ) %>%
    mutate(
        delay_deviation_pct_ci = qt(1 - conf_alpha / 2, n_runs - 1) * delay_deviation_pct_sd / sqrt(n_runs),
        jitter_deviation_pct_ci = qt(1 - conf_alpha / 2, n_runs - 1) * jitter_deviation_pct_sd / sqrt(n_runs),
        theoretical_passage_time_mean_ns_ci = qt(1 - conf_alpha / 2, n_runs - 1) * theoretical_passage_time_mean_ns_sd / sqrt(n_runs)
    )

##################################################################################################
# Plots: delay and jitter deviation across all half-life periods

# Convert rate to factor for categorical x-axis with automatic dodging
df_aggregated$rate_config <- factor(df_aggregated$rate_config,
    levels = c("0.1Gbps", "1Gbps", "10Gbps", "100Gbps"),
    labels = c("0.1", "1", "10", "100")
)

# Map rate factor to numeric x positions with extra spacing between groups
rate_levels <- levels(df_aggregated$rate_config)
group_spacing <- 1.25
rate_positions <- 1 + (seq_along(rate_levels) - 1) * group_spacing
df_aggregated$rate_x <- rate_positions[as.integer(df_aggregated$rate_config)]
group_dividers <- rate_positions[-1] - group_spacing / 2

dodge <- position_dodge(width = 0.9)

# Base theme
base_theme <- theme_minimal() +
    theme(
        panel.grid.minor = element_blank(),
        panel.grid.major = element_blank(),
        axis.text = element_text(size = font_size),
        axis.title = element_text(size = font_size, face = "bold"),
        axis.line = element_line(),
        axis.ticks = element_line(),
        legend.position = "bottom",
        legend.text = element_text(size = font_size),
        legend.title = element_text(size = font_size, face = "italic"),
        plot.title = element_text(size = font_size, face = "bold", hjust = 0.5),
        plot.margin = margin(t = 25, r = 10, b = 5, l = 5)
    )

legend_theme <- theme(legend.position = "bottom", legend.box = "vertical")

write.csv(df_aggregated, "half_life_statistics.csv", row.names = FALSE)
cat("\nStatistics saved as: half_life_statistics.csv\n")

for (dist in distributions) {
    df_dist <- df_aggregated %>% filter(distribution == dist)

    # Plot 1: Mean Delay Deviation
    plot_delay <- ggplot(df_dist, aes(
        x = rate_x,
        y = delay_deviation_pct_avg,
        color = half_life,
        shape = jitter_config,
        group = interaction(jitter_config, half_life)
    )) +
        geom_vline(xintercept = group_dividers, linetype = "dashed", color = "grey50") +
        geom_errorbar(
            aes(
                ymin = delay_deviation_pct_avg - delay_deviation_pct_ci,
                ymax = delay_deviation_pct_avg + delay_deviation_pct_ci
            ),
            width = geom_errorbar_width,
            linetype = "solid",
            linewidth = line_size,
            position = dodge
        ) +
        geom_point(size = point_size, position = dodge) +
        scale_x_continuous(breaks = rate_positions, labels = rate_levels) +
        scale_y_continuous(labels = label_number(big.mark = " ")) +
        scale_color_manual(values = half_life_colors) +
        scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
        guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
        base_theme +
        labs(
            x = "Rate (Gbit/s)",
            y = "Mean delay\ndeviation (%)",
            color = expression(t[h]),
            shape = "Jitter"
        )

    # Plot 2: Jitter Deviation
    plot_jitter <- ggplot(df_dist, aes(
        x = rate_x,
        y = jitter_deviation_pct_avg,
        color = half_life,
        shape = jitter_config,
        group = interaction(jitter_config, half_life)
    )) +
        geom_vline(xintercept = group_dividers, linetype = "dashed", color = "grey50") +
        geom_errorbar(
            aes(
                ymin = jitter_deviation_pct_avg - jitter_deviation_pct_ci,
                ymax = jitter_deviation_pct_avg + jitter_deviation_pct_ci
            ),
            width = geom_errorbar_width,
            linetype = "solid",
            linewidth = line_size,
            position = dodge
        ) +
        geom_point(size = point_size, position = dodge) +
        scale_x_continuous(breaks = rate_positions, labels = rate_levels) +
        scale_y_continuous(labels = label_number(big.mark = " ")) +
        scale_color_manual(values = half_life_colors) +
        scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
        guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
        base_theme +
        labs(
            x = "Rate (Gbit/s)",
            y = "Jitter\ndeviation (%)",
            color = expression(t[h]),
            shape = "Jitter"
        )

    # Plot 3: Theoretical Passage Time
    plot_passage_time <- ggplot(df_dist, aes(
        x = rate_x,
        y = theoretical_passage_time_mean_ns_avg / 1e6,
        color = half_life,
        shape = jitter_config,
        group = interaction(jitter_config, half_life)
    )) +
        geom_vline(xintercept = group_dividers, linetype = "dashed", color = "grey50") +
        geom_errorbar(
            aes(
                ymin = (theoretical_passage_time_mean_ns_avg - theoretical_passage_time_mean_ns_ci) / 1e6,
                ymax = (theoretical_passage_time_mean_ns_avg + theoretical_passage_time_mean_ns_ci) / 1e6
            ),
            width = geom_errorbar_width,
            linetype = "solid",
            linewidth = line_size,
            position = dodge
        ) +
        geom_point(size = point_size, position = dodge) +
        scale_x_continuous(breaks = rate_positions, labels = rate_levels) +
        scale_y_continuous(transform = "log10", labels = label_number(big.mark = " ")) +
        scale_color_manual(values = half_life_colors) +
        scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
        guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
        base_theme +
        labs(
            x = "Rate (Gbit/s)",
            y = "Passage time\n(ms)",
            color = expression(t[h]),
            shape = "Jitter"
        )

    ggsave(
        file = paste0("half_life_delay_", dist, ".pdf"),
        plot = plot_delay & legend_theme,
        device = cairo_pdf,
        width = 14.4, height = 6
    )
    cat(paste0("\nPlot saved as: half_life_delay_", dist, ".pdf\n"))

    ggsave(
        file = paste0("half_life_jitter_", dist, ".pdf"),
        plot = plot_jitter & legend_theme,
        device = cairo_pdf,
        width = 14.4, height = 6
    )
    cat(paste0("\nPlot saved as: half_life_jitter_", dist, ".pdf\n"))

    ggsave(
        file = paste0("half_life_passage_time_", dist, ".pdf"),
        plot = plot_passage_time & legend_theme,
        device = cairo_pdf,
        width = 14.4, height = 6
    )
    cat(paste0("\nPlot saved as: half_life_passage_time_", dist, ".pdf\n"))
}
