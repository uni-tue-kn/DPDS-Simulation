#!/usr/bin/env Rscript

library(ggplot2)
library(magrittr)
library(dplyr)
library(scales)
library(tidyr)
library(patchwork)

# Define the parameters
base_prefixes <- c("correlation30", "reorder")
spacings <- c(0, 50, 80, 99)
delays <- c("10ms")
jitters <- c("100us", "300us", "1000us", "3000us")
rates <- c("0.1Gbps", "1Gbps", "10Gbps", "100Gbps")
distributions <- c("normal")

# Plot parameters
font_size <- 16
line_size <- 1
point_size <- 4
conf_alpha <- 0.05
geom_errorbar_width <- 0.7

# Initialize an empty dataframe to store all results
df <- data.frame(
    base_prefix = character(),
    spacing = numeric(),
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

# Read all CSV files
for (base_prefix in base_prefixes) {
    for (spacing in spacings) {
        # Build the actual prefix
        if (spacing == 0) {
            prefix <- base_prefix
        } else {
            prefix <- paste0(base_prefix, "_spacer", spacing)
        }

        for (distribution in distributions) {
            for (delay in delays) {
                for (jitter in jitters) {
                    for (rate in rates) {
                        filename <- paste0(prefix, "_", distribution, "_", delay, "_", jitter, "_", rate, ".csv")
                        filepath <- filename

                        # Check if file exists
                        if (file.exists(filepath)) {
                            # Read the CSV file
                            data <- read.csv(filepath)

                            # Add metadata columns
                            data$base_prefix <- base_prefix
                            data$spacing <- spacing
                            data$distribution <- distribution
                            data$delay_config <- delay
                            data$jitter_config <- jitter
                            data$rate_config <- rate
                            data$rate_numeric <- as.numeric(gsub("Gbps", "", rate))

                            # Append to df
                            df <- rbind(df, data)
                        } else {
                            warning(paste("File not found:", filename))
                        }
                    }
                }
            }
        }
    }
}

# To be displayed in that order in the legend
df$jitter_config <- factor(df$jitter_config,
    levels = c("100us", "300us", "1000us", "3000us"),
    labels = c("0.1 ms", "0.3 ms", "1 ms", "3 ms")
)

df$spacing_config <- factor(df$spacing,
    levels = c(0, 50, 80, 99),
    labels = c("0%", "50%", "80%", "99%")
)

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

# Calculate per-run absolute deviations in microseconds: (measured - configured)
df <- df %>%
    mutate(
        configured_delay_ns = as.numeric(gsub("ms", "", as.character(delay_config))) * 1e6,
        configured_jitter_ns = as.numeric(gsub("ms", "", as.character(jitter_config))) * 1e6,
        delay_deviation_ms = (delay_mean_ns - configured_delay_ns) / 1e6,
        jitter_deviation_ms = (delay_std_ns - configured_jitter_ns) / 1e6,
        delay_deviation_pct = (delay_mean_ns - configured_delay_ns) / configured_delay_ns * 100,
        jitter_deviation_pct = (delay_std_ns - configured_jitter_ns) / configured_jitter_ns * 100,
        theoretical_iat_ns = (1538 * 8) / rate_numeric,
        iat_deviation_pct = (inter_arrival_time_mean_ns - theoretical_iat_ns) / theoretical_iat_ns * 100,
        iat_cvar = inter_arrival_time_std_ns / inter_arrival_time_mean_ns
    )

# Aggregate statistics: calculate mean and standard deviation across all runs
df_aggregated <- df %>%
    group_by(base_prefix, spacing, spacing_config, distribution, delay_config, jitter_config, rate_config, rate_numeric) %>%
    summarise(
        delay_mean_ns_avg = mean(delay_mean_ns),
        delay_mean_ns_sd = sd(delay_mean_ns),
        delay_std_ns_avg = mean(delay_std_ns),
        delay_std_ns_sd = sd(delay_std_ns),
        inter_arrival_time_mean_ns_avg = mean(inter_arrival_time_mean_ns),
        inter_arrival_time_mean_ns_sd = sd(inter_arrival_time_mean_ns),
        inter_arrival_time_std_ns_avg = mean(inter_arrival_time_std_ns),
        inter_arrival_time_std_ns_sd = sd(inter_arrival_time_std_ns),
        kendall_tau_distance_div_n_avg = mean(kendall_tau_distance_div_n),
        kendall_tau_distance_div_n_sd = sd(kendall_tau_distance_div_n),
        out_of_order_packets_avg = mean(out_of_order_packets),
        out_of_order_packets_sd = sd(out_of_order_packets),
        theoretical_passage_time_mean_ns_avg = mean(theoretical_passage_time_mean_ns),
        theoretical_passage_time_mean_ns_sd = sd(theoretical_passage_time_mean_ns),
        measured_passage_time_mean_ns_avg = mean(measured_passage_time_mean_ns),
        measured_passage_time_mean_ns_sd = sd(measured_passage_time_mean_ns),
        delay_deviation_ms_avg = mean(delay_deviation_ms),
        delay_deviation_ms_sd = sd(delay_deviation_ms),
        jitter_deviation_ms_avg = mean(jitter_deviation_ms),
        jitter_deviation_ms_sd = sd(jitter_deviation_ms),
        delay_deviation_pct_avg = mean(delay_deviation_pct),
        delay_deviation_pct_sd = sd(delay_deviation_pct),
        jitter_deviation_pct_avg = mean(jitter_deviation_pct),
        jitter_deviation_pct_sd = sd(jitter_deviation_pct),
        iat_deviation_pct_avg = mean(iat_deviation_pct),
        iat_deviation_pct_sd = sd(iat_deviation_pct),
        iat_cvar_avg = mean(iat_cvar),
        iat_cvar_sd = sd(iat_cvar),
        dropped_packets_bandwidth_avg = mean(dropped_packets_bandwidth),
        dropped_packets_bandwidth_sd = sd(dropped_packets_bandwidth),
        burst_spacing_offset_mean_ns_avg = mean(burst_spacing_offset_mean_ns),
        burst_spacing_offset_mean_ns_sd = sd(burst_spacing_offset_mean_ns),
        burst_spacing_offset_max_ns_avg = mean(burst_spacing_offset_max_ns),
        burst_spacing_offset_max_ns_sd = sd(burst_spacing_offset_max_ns),
        n_runs = n(),
        .groups = "drop"
    )

# Calculate confidence interval half-sizes
df_aggregated <- df_aggregated %>%
    mutate(
        delay_deviation_ms_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (delay_deviation_ms_sd / sqrt(n_runs)),
        jitter_deviation_ms_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (jitter_deviation_ms_sd / sqrt(n_runs)),
        delay_deviation_pct_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (delay_deviation_pct_sd / sqrt(n_runs)),
        jitter_deviation_pct_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (jitter_deviation_pct_sd / sqrt(n_runs))
    )

##################################################################################################
# Plotting

# Convert rate to factor for categorical x-axis with automatic dodging
df_aggregated$rate_config <- factor(df_aggregated$rate_config,
    levels = c("0.1Gbps", "1Gbps", "10Gbps", "100Gbps"),
    labels = c("0.1", "1", "10", "100")
)

# Map rate factor to numeric x positions with extra spacing between groups
rate_levels <- levels(df_aggregated$rate_config)
group_spacing <- 1.1
rate_positions <- 1 + (seq_along(rate_levels) - 1) * group_spacing
df_aggregated$rate_x <- rate_positions[as.integer(df_aggregated$rate_config)]
group_dividers <- rate_positions[-1] - group_spacing / 2

dodge <- position_dodge(width = 1.0)

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
        plot.title = element_text(size = font_size, face = "bold", hjust = 0.5)
    )

legend_theme <- theme(legend.position = "bottom", legend.box = "vertical")

for (dist in distributions) {
    for (bp in base_prefixes) {
        df_dist <- df_aggregated %>% filter(distribution == dist, base_prefix == bp)

        # Plot 1: Mean Delay Deviation (%)
        plot_delay <- ggplot(df_dist, aes(
            x = rate_x,
            y = delay_deviation_pct_avg,
            color = spacing_config,
            shape = jitter_config,
            group = interaction(jitter_config, spacing_config)
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
            scale_color_manual(values = c("0%" = "#000000", "50%" = "#0072B2", "80%" = "#D55E00", "99%" = "#E69F00")) +
            scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
            guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
            base_theme +
            legend_theme +
            labs(
                x = "Rate (Gbit/s)",
                y = "Mean delay deviation (%)",
                color = "Utilization",
                shape = "Jitter"
            )

        ggsave(
            file = paste0("spacing_delay_", bp, "_", dist, ".pdf"),
            plot = plot_delay,
            device = cairo_pdf,
            width = 12, height = 5
        )
        cat(paste0("\nPlot saved as: spacing_delay_", bp, "_", dist, ".pdf\n"))

        # Plot 2: Jitter Deviation (%)
        plot_jitter <- ggplot(df_dist, aes(
            x = rate_x,
            y = jitter_deviation_pct_avg,
            color = spacing_config,
            shape = jitter_config,
            group = interaction(jitter_config, spacing_config)
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
            expand_limits(y = 0) +
            scale_color_manual(values = c("0%" = "#000000", "50%" = "#0072B2", "80%" = "#D55E00", "99%" = "#E69F00")) +
            scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
            guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
            base_theme +
            legend_theme +
            labs(
                x = "Rate (Gbit/s)",
                y = "Jitter deviation (%)",
                color = "Utilization",
                shape = "Jitter"
            )

        ggsave(
            file = paste0("spacing_jitter_", bp, "_", dist, ".pdf"),
            plot = plot_jitter,
            device = cairo_pdf,
            width = 12, height = 5
        )
        cat(paste0("\nPlot saved as: spacing_jitter_", bp, "_", dist, ".pdf\n"))
    }
}
