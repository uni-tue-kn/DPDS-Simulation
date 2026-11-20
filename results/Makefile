# Runs every plotting R-script in this directory, one after another.
#
# Usage:
#   make            # run all scripts sequentially
#   make plot_delays.r   # run a single script
#   make clean      # remove generated PDFs and aggregated CSVs

RSCRIPT ?= Rscript

# All R-scripts, in execution order.
SCRIPTS := \
	plot_distributions.r \
	plot_delays.r \
	plot_reordering.r \
	plot_spacing.r \
	plot_half_life.r \
	plot_results.r

.PHONY: all clean $(SCRIPTS)

# Run the scripts strictly one after another, even under `make -j`.
.NOTPARALLEL:

all: $(SCRIPTS)

$(SCRIPTS):
	@echo "==> Running $@"
	$(RSCRIPT) $@

clean:
	rm -f *.pdf \
		distributions_aggregated.csv \
		distributions_sampling_cost_aggregated.csv \
		half_life_statistics.csv \
		*_aggregated_statistics.csv
