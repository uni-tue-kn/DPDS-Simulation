#!/usr/bin/env Rscript

library(ggplot2)
library(magrittr)
library(dplyr)
library(scales)
library(tidyr)
library(patchwork)

# Define the parameters
prefixes <- c("basic", "reorder", "correlation30")
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

# Initialize an empty dataframe to store all results
df <- data.frame(
    prefix = character(),
    distribution = character(),
    delay_config = character(),
    jitter_config = character(),
    rate_config = character(),
    rate_numeric = numeric(),
    delay_mean_ns = numeric(),
    delay_std_ns = numeric()
)

# Read all CSV files
for (prefix in prefixes) {
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
                        data$prefix <- prefix
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

# To be displayed in that order in the legend
df$jitter_config <- factor(df$jitter_config,
    levels = c("100us", "300us", "1000us", "3000us"),
    labels = c("0.1 ms", "0.3 ms", "1 ms", "3 ms")
)

##################################################################################################
# Sanity checks: warn if any packets were dropped

dropped_lim <- df %>% filter(dropped_packets_limiter > 0)
if (nrow(dropped_lim) > 0) {
    run_details <- paste(
        apply(dropped_lim, 1, function(r) paste0(
            "  ", r["prefix"], "/", r["distribution"], "/", r["delay_config"], "/",
            r["jitter_config"], "/", r["rate_config"],
            " (dropped: ", r["dropped_packets_limiter"], ")"
        )),
        collapse = "\n"
    )
    warning(paste0(
        nrow(dropped_lim), " run(s) have dropped_packets_limiter > 0 ",
        "(total dropped: ", sum(dropped_lim$dropped_packets_limiter), "):\n",
        run_details
    ))
}

dropped_full <- df %>% filter(dropped_packets_buffer_full > 0)
if (nrow(dropped_full) > 0) {
    run_details <- paste(
        apply(dropped_full, 1, function(r) paste0(
            "  ", r["prefix"], "/", r["distribution"], "/", r["delay_config"], "/",
            r["jitter_config"], "/", r["rate_config"],
            " (dropped: ", r["dropped_packets_buffer_full"], ")"
        )),
        collapse = "\n"
    )
    warning(paste0(
        nrow(dropped_full), " run(s) have dropped_packets_buffer_full > 0 ",
        "(dropped because the buffer was full, total dropped: ", sum(dropped_full$dropped_packets_buffer_full), "). ",
        "These are not valid runs; re-run them with a larger buffer_capacity:\n",
        run_details
    ))
}

# Calculate per-run deviation percentages: (measured - configured) / configured * 100
df <- df %>%
    mutate(
        configured_delay_ns = as.numeric(gsub("ms", "", delay_config)) * 1e6,
        configured_jitter_ns = as.numeric(gsub("ms", "", as.character(jitter_config))) * 1e6,
        delay_deviation_pct = (delay_mean_ns - configured_delay_ns) / configured_delay_ns * 100,
        jitter_deviation_pct = (delay_std_ns - configured_jitter_ns) / configured_jitter_ns * 100
    )

# Aggregate statistics: calculate mean and standard deviation across all runs
df_aggregated <- df %>%
    group_by(prefix, distribution, delay_config, jitter_config, rate_config, rate_numeric) %>%
    summarise(
        delay_mean_ns_avg = mean(delay_mean_ns),
        delay_mean_ns_sd = sd(delay_mean_ns),
        delay_std_ns_avg = mean(delay_std_ns),
        delay_std_ns_sd = sd(delay_std_ns),
        kendall_tau_distance_div_n_avg = mean(kendall_tau_distance_div_n),
        kendall_tau_distance_div_n_sd = sd(kendall_tau_distance_div_n),
        out_of_order_packets_avg = mean(out_of_order_packets),
        out_of_order_packets_sd = sd(out_of_order_packets),
        theoretical_passage_time_mean_ns_avg = mean(theoretical_passage_time_mean_ns),
        theoretical_passage_time_mean_ns_sd = sd(theoretical_passage_time_mean_ns),
        delay_deviation_pct_avg = mean(delay_deviation_pct),
        delay_deviation_pct_sd = sd(delay_deviation_pct),
        jitter_deviation_pct_avg = mean(jitter_deviation_pct),
        jitter_deviation_pct_sd = sd(jitter_deviation_pct),
        dropped_packets_limiter_avg = mean(dropped_packets_limiter),
        dropped_packets_limiter_sd = sd(dropped_packets_limiter),
        n_runs = n(),
        .groups = "drop"
    )

# Calculate confidence interval half-sizes (will use conf_alpha defined later)
df_aggregated <- df_aggregated %>%
    mutate(
        delay_mean_ns_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (delay_mean_ns_sd / sqrt(n_runs)),
        delay_std_ns_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (delay_std_ns_sd / sqrt(n_runs)),
        kendall_tau_distance_div_n_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (kendall_tau_distance_div_n_sd / sqrt(n_runs)),
        out_of_order_packets_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (out_of_order_packets_sd / sqrt(n_runs)),
        theoretical_passage_time_mean_ns_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (theoretical_passage_time_mean_ns_sd / sqrt(n_runs)),
        delay_deviation_pct_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (delay_deviation_pct_sd / sqrt(n_runs)),
        jitter_deviation_pct_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (jitter_deviation_pct_sd / sqrt(n_runs)),
        dropped_packets_limiter_ci = qt(1 - conf_alpha / 2, n_runs - 1) * (dropped_packets_limiter_sd / sqrt(n_runs))
    )

##################################################################################################
# Export aggregated statistics to CSV

# Export separate CSV files for each prefix and distribution (must be done before factor conversion)
for (p in prefixes) {
    for (dist in distributions) {
        df_prefix_export <- df_aggregated %>%
            filter(prefix == p, distribution == dist)

        csv_filename <- paste0(p, "_", dist, "_aggregated_statistics.csv")
        write.csv(df_prefix_export, file = csv_filename, row.names = FALSE)
        cat(paste0("\nAggregated statistics exported to: ", csv_filename, "\n"))
    }
}

##################################################################################################
# Combined Plot: delay and jitter across all prefixes

# Order prefixes for consistent legend
df_aggregated$prefix <- factor(df_aggregated$prefix,
    levels = c("basic", "reorder", "correlation30"),
    labels = c("Uncorrelated", "Reordered", "Correlated")
)

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

dodge <- position_dodge(width = 0.7)

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
        legend.location = "plot",
        legend.text = element_text(size = font_size, margin = margin(l = 8)),
        legend.title = element_text(size = font_size, face = "italic", margin = margin(r = 8)),
        legend.key.spacing.x = unit(10, "pt"),
        plot.title = element_text(size = font_size, face = "bold", hjust = 0.5),
        plot.margin = margin(t = 25, r = 10, b = 5, l = 5)
    )

legend_theme <- theme(legend.position = "bottom", legend.box = "vertical")

for (dist in distributions) {
    df_dist <- df_aggregated %>% filter(distribution == dist)

    # Plot 1: Mean Delay Deviation
    plot_delay <- ggplot(df_dist, aes(
        x = rate_x,
        y = delay_deviation_pct_avg,
        color = prefix,
        shape = jitter_config,
        group = interaction(jitter_config, prefix)
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
        scale_color_manual(values = c(Uncorrelated = "#000000", Reordered = "#000000", Correlated = "#0072B2")) +
        scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
        guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
        base_theme +
        labs(
            x = "Rate (Gbit/s)",
            y = "Mean delay\ndeviation (%)",
            color = "Configuration",
            shape = "Jitter"
        )

    # Plot 2: Jitter Deviation
    plot_jitter <- ggplot(df_dist, aes(
        x = rate_x,
        y = jitter_deviation_pct_avg,
        color = prefix,
        shape = jitter_config,
        group = interaction(jitter_config, prefix)
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
        scale_color_manual(values = c(Uncorrelated = "#000000", Reordered = "#000000", Correlated = "#0072B2")) +
        scale_shape_manual(values = c("0.1 ms" = 16, "0.3 ms" = 17, "1 ms" = 15, "3 ms" = 18)) +
        guides(shape = guide_legend(order = 1), color = guide_legend(order = 2, override.aes = list(shape = NA, linewidth = 2))) +
        base_theme +
        labs(
            x = "Rate (Gbit/s)",
            y = "Jitter\ndeviation (%)",
            color = "Configuration",
            shape = "Jitter"
        )

    # Group 1: Uncorrelated (basic)
    df_dist_group1 <- df_dist %>% filter(prefix %in% c("Uncorrelated"))
    # Group 2: Reordered + Correlated (reorder + correlation30)
    df_dist_group2 <- df_dist %>% filter(prefix %in% c("Reordered", "Correlated"))

    for (group_info in list(
        list(data = df_dist_group1, suffix = "uncorrelated", hide_color = TRUE),
        list(data = df_dist_group2, suffix = "reordered_correlated", hide_color = FALSE)
    )) {
        extra_guides <- if (group_info$hide_color) guides(color = "none") else NULL

        ggsave(
            file = paste0("combined_delay_", dist, "_", group_info$suffix, ".pdf"),
            plot = (plot_delay %+% group_info$data) & legend_theme & extra_guides,
            device = cairo_pdf,
            width = 14.4, height = 6
        )
        cat(paste0("\nPlot saved as: combined_delay_", dist, "_", group_info$suffix, ".pdf\n"))

        ggsave(
            file = paste0("combined_jitter_", dist, "_", group_info$suffix, ".pdf"),
            plot = (plot_jitter %+% group_info$data) & legend_theme & extra_guides,
            device = cairo_pdf,
            width = 14.4, height = 6
        )
        cat(paste0("\nPlot saved as: combined_jitter_", dist, "_", group_info$suffix, ".pdf\n"))
    }
}
